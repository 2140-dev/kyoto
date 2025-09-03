use std::{ops::DerefMut, sync::Arc, time::Duration};

use bitcoin::{
    block::Header,
    p2p::{
        message_filter::{CFHeaders, CFilter},
        message_network::VersionMessage,
        ServiceFlags,
    },
    Block, BlockHash, Network, ScriptBuf,
};
use tokio::sync::{
    mpsc::{Receiver, UnboundedReceiver},
    Mutex, RwLock,
};
use tokio::{
    select,
    sync::mpsc::{self},
};

use crate::{
    chain::{
        block_queue::{BlockQueue, BlockRecipient, ProcessBlockResponse},
        chain::Chain,
        checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
        error::{CFilterSyncError, HeaderSyncError},
        CFHeaderChanges, FilterCheck, HeaderChainChanges, HeightMonitor,
    },
    db::traits::PeerStore,
    error::FetchBlockError,
    network::{peer_map::PeerMap, LastBlockMonitor, PeerId},
    IndexedBlock, NodeState, TxBroadcast, TxBroadcastPolicy,
};

use super::{
    channel_messages::{GetHeaderConfig, MainThreadMessage, PeerMessage, PeerThreadMessage},
    client::Client,
    config::NodeConfig,
    dialog::Dialog,
    error::NodeError,
    messages::{ClientMessage, Event, Info, SyncUpdate, Warning},
};

pub(crate) const WTXID_VERSION: u32 = 70016;
const LOOP_TIMEOUT: Duration = Duration::from_secs(1);

type PeerRequirement = usize;

/// A compact block filter node. Nodes download Bitcoin block headers, block filters, and blocks to send relevant events to a client.
#[derive(Debug)]
pub struct Node<P: PeerStore + 'static> {
    state: Arc<RwLock<NodeState>>,
    chain: Arc<Mutex<Chain>>,
    peer_map: Arc<Mutex<PeerMap<P>>>,
    required_peers: PeerRequirement,
    dialog: Arc<Dialog>,
    block_queue: Arc<Mutex<BlockQueue>>,
    client_recv: Arc<Mutex<UnboundedReceiver<ClientMessage>>>,
    peer_recv: Arc<Mutex<Receiver<PeerThreadMessage>>>,
}

impl<P: PeerStore> Node<P> {
    pub(crate) fn new(network: Network, config: NodeConfig, peer_store: P) -> (Self, Client) {
        let NodeConfig {
            required_peers,
            white_list,
            dns_resolver,
            addresses,
            data_path: _,
            header_checkpoint,
            connection_type,
            target_peer_size,
            peer_timeout_config,
            log_level,
        } = config;
        // Set up a communication channel between the node and client
        let (log_tx, log_rx) = mpsc::channel::<String>(32);
        let (info_tx, info_rx) = mpsc::channel::<Info>(32);
        let (warn_tx, warn_rx) = mpsc::unbounded_channel::<Warning>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (ctx, crx) = mpsc::unbounded_channel::<ClientMessage>();
        let client = Client::new(log_rx, info_rx, warn_rx, event_rx, ctx);
        // A structured way to talk to the client
        let dialog = Arc::new(Dialog::new(log_level, log_tx, info_tx, warn_tx, event_tx));
        // We always assume we are behind
        let state = Arc::new(RwLock::new(NodeState::Behind));
        // Configure the peer manager
        let (mtx, mrx) = mpsc::channel::<PeerThreadMessage>(32);
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let peer_map = Arc::new(Mutex::new(PeerMap::new(
            mtx,
            network,
            peer_store,
            white_list,
            Arc::clone(&dialog),
            connection_type,
            target_peer_size,
            peer_timeout_config,
            Arc::clone(&height_monitor),
            dns_resolver,
        )));
        // Prepare the header checkpoints for the chain source
        let mut checkpoints = HeaderCheckpoints::new(&network);
        let checkpoint = header_checkpoint.unwrap_or_else(|| checkpoints.last());
        checkpoints.prune_up_to(checkpoint);
        // Build the chain
        let chain = Chain::new(
            network,
            addresses,
            checkpoint,
            checkpoints,
            Arc::clone(&dialog),
            height_monitor,
            required_peers,
        );
        let chain = Arc::new(Mutex::new(chain));
        (
            Self {
                state,
                chain,
                peer_map,
                required_peers: required_peers.into(),
                dialog,
                block_queue: Arc::new(Mutex::new(BlockQueue::new())),
                client_recv: Arc::new(Mutex::new(crx)),
                peer_recv: Arc::new(Mutex::new(mrx)),
            },
            client,
        )
    }

    /// Run the node continuously. Typically run on a separate thread than the underlying application.
    ///
    /// # Errors
    ///
    /// A node will cease running if a fatal error is encountered with either the [`PeerStore`] or [`HeaderStore`].
    pub async fn run(&self) -> Result<(), NodeError<P::Error>> {
        crate::log!(self.dialog, "Starting node");
        crate::log!(
            self.dialog,
            format!(
                "Configured connection requirement: {} peers",
                self.required_peers
            )
        );
        let mut last_block = LastBlockMonitor::new();
        let mut peer_recv = self.peer_recv.lock().await;
        let mut client_recv = self.client_recv.lock().await;
        loop {
            // Try to advance the state of the node
            self.advance_state(&mut last_block).await;
            // Connect to more peers if we need them and remove old connections
            self.dispatch().await?;
            // If there are blocks we need in the queue, we should request them of a random peer
            self.get_blocks().await;
            // Either handle a message from a remote peer or from our client
            select! {
                peer = tokio::time::timeout(LOOP_TIMEOUT, peer_recv.recv()) => {
                    match peer {
                        Ok(Some(peer_thread)) => {
                            match peer_thread.message {
                                PeerMessage::Version(version) => {
                                    {
                                        let mut peer_map = self.peer_map.lock().await;
                                        peer_map.set_services(peer_thread.nonce, version.services);
                                        peer_map.set_height(peer_thread.nonce, version.start_height as u32).await;
                                    }
                                    let response = self.handle_version(peer_thread.nonce, version).await?;
                                    self.send_message(peer_thread.nonce, response).await;
                                    crate::log!(self.dialog, format!("[{}]: version", peer_thread.nonce));
                                }
                                PeerMessage::Headers(headers) => {
                                    last_block.reset();
                                    crate::log!(self.dialog, format!("[{}]: headers", peer_thread.nonce));
                                    match self.handle_headers(peer_thread.nonce, headers).await {
                                        Some(response) => {
                                            self.send_message(peer_thread.nonce, response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::FilterHeaders(cf_headers) => {
                                    crate::log!(self.dialog, format!("[{}]: filter headers", peer_thread.nonce));
                                    match self.handle_cf_headers(peer_thread.nonce, cf_headers).await {
                                        Some(response) => {
                                            self.broadcast(response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::Filter(filter) => {
                                    match self.handle_filter(peer_thread.nonce, filter).await {
                                        Some(response) => {
                                            self.send_message(peer_thread.nonce, response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::Block(block) => match self.handle_block(peer_thread.nonce, block).await {
                                    Some(response) => {
                                        self.send_message(peer_thread.nonce, response).await;
                                    }
                                    None => continue,
                                },
                                PeerMessage::NewBlocks(blocks) => {
                                    crate::log!(self.dialog, format!("[{}]: inv", peer_thread.nonce));
                                    match self.handle_inventory_blocks(peer_thread.nonce, blocks).await {
                                        Some(response) => {
                                            self.broadcast(response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::FeeFilter(feerate) => {
                                    let mut peer_map = self.peer_map.lock().await;
                                    peer_map.set_broadcast_min(peer_thread.nonce, feerate);
                                }
                            }
                        },
                        _ => continue,
                    }
                },
                message = client_recv.recv() => {
                    if let Some(message) = message {
                        match message {
                            ClientMessage::Shutdown => return Ok(()),
                            ClientMessage::Broadcast(transaction) => {
                                self.broadcast_transaction(transaction).await;
                            },
                            ClientMessage::AddScript(script) =>  self.add_script(script).await,
                            ClientMessage::Rescan => {
                                if let Some(response) = self.rescan().await {
                                    self.broadcast(response).await;
                                }
                            },
                            ClientMessage::GetBlock(request) => {
                                let chain = self.chain.lock().await;
                                let mut queue = self.block_queue.lock().await;
                                let height_opt = chain.header_chain.height_of_hash(request.hash);
                                if height_opt.is_none() {
                                    let err_reponse = request.oneshot.send(Err(FetchBlockError::UnknownHash));
                                    if err_reponse.is_err() {
                                        self.dialog.send_warning(Warning::ChannelDropped);
                                    }
                                } else {
                                    crate::log!(
                                        self.dialog,
                                        format!("Adding block {} to queue", request.hash)
                                    );
                                    queue.add(request);
                                }
                            },
                            ClientMessage::SetDuration(duration) => {
                                let mut peer_map = self.peer_map.lock().await;
                                peer_map.set_duration(duration);
                            },
                            ClientMessage::AddPeer(peer) => {
                                let mut peer_map = self.peer_map.lock().await;
                                peer_map.add_trusted_peer(peer);
                            },
                            ClientMessage::GetBroadcastMinFeeRate(request) => {
                                let peer_map = self.peer_map.lock().await;
                                let fee_rate = peer_map.broadcast_min();
                                let send_result = request.send(fee_rate);
                                if send_result.is_err() {
                                    self.dialog.send_warning(Warning::ChannelDropped);
                                };
                            }
                            ClientMessage::NoOp => (),
                        }
                    }
                }
            }
        }
    }

    // Send a message to a specified peer
    async fn send_message(&self, nonce: PeerId, message: MainThreadMessage) {
        let mut peer_map = self.peer_map.lock().await;
        peer_map.send_message(nonce, message).await;
    }

    // Broadcast a message to all connected peers
    async fn broadcast(&self, message: MainThreadMessage) {
        let mut peer_map = self.peer_map.lock().await;
        peer_map.broadcast(message).await;
    }

    // Send a message to a random peer
    async fn send_random(&self, message: MainThreadMessage) {
        let mut peer_map = self.peer_map.lock().await;
        peer_map.send_random(message).await;
    }

    // Connect to a new peer if we are not connected to enough
    async fn dispatch(&self) -> Result<(), NodeError<P::Error>> {
        let mut peer_map = self.peer_map.lock().await;
        peer_map.clean().await;
        let live = peer_map.live();
        let required = self.next_required_peers().await;
        // Find more peers when lower than the desired threshold.
        if live < required {
            self.dialog.send_warning(Warning::NeedConnections {
                connected: live,
                required,
            });
            let address = peer_map.next_peer().await?;
            if peer_map.dispatch(address).await.is_err() {
                self.dialog.send_warning(Warning::CouldNotConnect);
            }
        }
        Ok(())
    }

    // If there are blocks in the queue, we should request them of a random peer
    async fn get_blocks(&self) {
        if let Some(block_request) = self.pop_block_queue().await {
            crate::log!(self.dialog, "Sending block request to random peer");
            self.send_random(block_request).await;
        }
    }

    // Broadcast transactions according to the configured policy
    async fn broadcast_transaction(&self, broadcast: TxBroadcast) {
        let mut peer_map = self.peer_map.lock().await;
        let mut queue = peer_map.tx_queue.lock().await;
        queue.add_to_queue(broadcast.tx);
        drop(queue);
        match broadcast.broadcast_policy {
            TxBroadcastPolicy::AllPeers => {
                crate::log!(
                    self.dialog,
                    format!("Sending transaction to {} connected peers", peer_map.live())
                );
                peer_map
                    .broadcast(MainThreadMessage::BroadcastPending)
                    .await
            }
            TxBroadcastPolicy::RandomPeer => {
                crate::log!(self.dialog, "Sending transaction to a random peer");
                peer_map
                    .send_random(MainThreadMessage::BroadcastPending)
                    .await
            }
        };
    }

    // Try to continue with the syncing process
    async fn advance_state(&self, last_block: &mut LastBlockMonitor) {
        let mut state = self.state.write().await;
        match *state {
            NodeState::Behind => {
                let header_chain = self.chain.lock().await;
                if header_chain.is_synced().await {
                    crate::info!(self.dialog, Info::StateChange(NodeState::HeadersSynced));
                    *state = NodeState::HeadersSynced;
                }
            }
            NodeState::HeadersSynced => {
                let header_chain = self.chain.lock().await;
                if header_chain.is_cf_headers_synced() {
                    crate::info!(
                        self.dialog,
                        Info::StateChange(NodeState::FilterHeadersSynced)
                    );
                    *state = NodeState::FilterHeadersSynced;
                }
            }
            NodeState::FilterHeadersSynced => {
                let header_chain = self.chain.lock().await;
                if header_chain.is_filters_synced() {
                    crate::info!(self.dialog, Info::StateChange(NodeState::FiltersSynced));
                    *state = NodeState::FiltersSynced;
                }
            }
            NodeState::FiltersSynced => {
                let queue = self.block_queue.lock().await;
                if queue.complete() {
                    let chain = self.chain.lock().await;
                    *state = NodeState::TransactionsSynced;
                    let update = SyncUpdate::new(
                        HeaderCheckpoint::new(
                            chain.header_chain.height(),
                            chain.header_chain.tip_hash(),
                        ),
                        chain.last_ten(),
                    );
                    crate::info!(
                        self.dialog,
                        Info::StateChange(NodeState::TransactionsSynced)
                    );
                    self.dialog.send_event(Event::Synced(update));
                }
            }
            NodeState::TransactionsSynced => {
                if last_block.stale() {
                    self.dialog.send_warning(Warning::PotentialStaleTip);
                    crate::log!(
                        self.dialog,
                        "Disconnecting from remote nodes to find new connections"
                    );
                    self.broadcast(MainThreadMessage::Disconnect).await;
                    last_block.reset();
                }
            }
        }
    }

    // When syncing headers we are only interested in one peer to start
    async fn next_required_peers(&self) -> PeerRequirement {
        let state = self.state.read().await;
        match *state {
            NodeState::Behind => 1,
            _ => self.required_peers,
        }
    }

    // After we receiving some chain-syncing message, we decide what chain of data needs to be
    // requested next.
    async fn next_stateful_message(&self, chain: &mut Chain) -> Option<MainThreadMessage> {
        if !chain.is_synced().await {
            let headers = GetHeaderConfig {
                locators: chain.header_chain.locators(),
                stop_hash: None,
            };
            return Some(MainThreadMessage::GetHeaders(headers));
        } else if !chain.is_cf_headers_synced() {
            return Some(MainThreadMessage::GetFilterHeaders(
                chain.next_cf_header_message(),
            ));
        } else if !chain.is_filters_synced() {
            return Some(MainThreadMessage::GetFilters(chain.next_filter_message()));
        }
        None
    }

    // We accepted a handshake with a peer but we may disconnect if they do not support CBF
    async fn handle_version(
        &self,
        nonce: PeerId,
        version_message: VersionMessage,
    ) -> Result<MainThreadMessage, NodeError<P::Error>> {
        if version_message.version < WTXID_VERSION {
            return Ok(MainThreadMessage::Disconnect);
        }
        let state = self.state.read().await;
        match *state {
            NodeState::Behind => (),
            _ => {
                if !version_message.services.has(ServiceFlags::COMPACT_FILTERS)
                    || !version_message.services.has(ServiceFlags::NETWORK)
                {
                    self.dialog.send_warning(Warning::NoCompactFilters);
                    return Ok(MainThreadMessage::Disconnect);
                }
            }
        }
        let mut peer_map = self.peer_map.lock().await;
        peer_map.tried(nonce).await;
        let needs_peers = peer_map.need_peers().await?;
        // First we signal for ADDRV2 support
        peer_map
            .send_message(nonce, MainThreadMessage::GetAddrV2)
            .await;
        // Then for BIP 339 witness transaction broadcast
        peer_map
            .send_message(nonce, MainThreadMessage::WtxidRelay)
            .await;
        peer_map
            .send_message(nonce, MainThreadMessage::Verack)
            .await;
        // Now we may request peers if required
        if needs_peers {
            crate::log!(self.dialog, "Requesting new addresses");
            peer_map
                .send_message(nonce, MainThreadMessage::GetAddr)
                .await;
        }
        // Inform the user we are connected to all required peers
        if peer_map.live().eq(&self.required_peers) {
            crate::info!(self.dialog, Info::ConnectionsMet);
        }
        // Even if we start the node as caught up in terms of height, we need to check for reorgs. So we can send this unconditionally.
        let chain = self.chain.lock().await;
        let next_headers = GetHeaderConfig {
            locators: chain.header_chain.locators(),
            stop_hash: None,
        };
        Ok(MainThreadMessage::GetHeaders(next_headers))
    }

    // We always send headers to our peers, so our next message depends on our state
    async fn handle_headers(
        &self,
        peer_id: PeerId,
        headers: Vec<Header>,
    ) -> Option<MainThreadMessage> {
        let mut chain = self.chain.lock().await;
        match chain.sync_chain(headers).await {
            Ok(changes) => match changes {
                HeaderChainChanges::Extended(height) => {
                    crate::info!(self.dialog, Info::NewChainHeight(height));
                }
                HeaderChainChanges::Reorg { height, hashes } => {
                    let mut queue = self.block_queue.lock().await;
                    queue.remove(&hashes);
                    for hash in hashes {
                        crate::log!(self.dialog, format!("{hash} was reorganized"));
                    }
                    crate::log!(self.dialog, format!("New chain height: {height}"));
                }
                HeaderChainChanges::ForkAdded { tip } => {
                    crate::log!(
                        self.dialog,
                        format!("Candidate fork {} -> {}", tip.height, tip.block_hash())
                    );
                    crate::info!(self.dialog, Info::NewFork { tip });
                }
                HeaderChainChanges::Duplicate => (),
            },
            Err(e) => match e {
                HeaderSyncError::EmptyMessage => {
                    if !chain.is_synced().await {
                        return Some(MainThreadMessage::Disconnect);
                    }
                    return self.next_stateful_message(chain.deref_mut()).await;
                }
                _ => {
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: format!("Unexpected header syncing error: {e}"),
                    });
                    let mut lock = self.peer_map.lock().await;
                    lock.ban(peer_id).await;
                    return Some(MainThreadMessage::Disconnect);
                }
            },
        }
        self.next_stateful_message(chain.deref_mut()).await
    }

    // Compact filter headers may result in a number of outcomes, including the need to audit filters.
    async fn handle_cf_headers(
        &self,
        peer_id: PeerId,
        cf_headers: CFHeaders,
    ) -> Option<MainThreadMessage> {
        let mut chain = self.chain.lock().await;
        chain.send_chain_update().await;
        match chain.sync_cf_headers(peer_id, cf_headers) {
            Ok(potential_message) => match potential_message {
                CFHeaderChanges::AddedToQueue => None,
                CFHeaderChanges::Extended => self.next_stateful_message(chain.deref_mut()).await,
                CFHeaderChanges::Conflict => {
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: "Found a conflict while peers are sending filter headers".into(),
                    });
                    Some(MainThreadMessage::Disconnect)
                }
            },
            Err(e) => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: format!("Compact filter header syncing encountered an error: {e}"),
                });
                let mut lock = self.peer_map.lock().await;
                lock.ban(peer_id).await;
                Some(MainThreadMessage::Disconnect)
            }
        }
    }

    // Handle a new compact block filter
    async fn handle_filter(&self, peer_id: PeerId, filter: CFilter) -> Option<MainThreadMessage> {
        let mut chain = self.chain.lock().await;
        match chain.sync_filter(filter) {
            Ok(potential_message) => {
                let FilterCheck {
                    needs_request,
                    was_last_in_batch,
                } = potential_message;
                if let Some(hash) = needs_request {
                    let mut queue = self.block_queue.lock().await;
                    queue.add(hash);
                }
                if was_last_in_batch {
                    chain.send_chain_update().await;
                    if !chain.is_filters_synced() {
                        let next_filters = chain.next_filter_message();
                        return Some(MainThreadMessage::GetFilters(next_filters));
                    }
                }
                None
            }
            Err(e) => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: format!("Compact filter syncing encountered an error: {e}"),
                });
                match e {
                    CFilterSyncError::Filter(_) => Some(MainThreadMessage::Disconnect),
                    _ => {
                        let mut lock = self.peer_map.lock().await;
                        lock.ban(peer_id).await;
                        Some(MainThreadMessage::Disconnect)
                    }
                }
            }
        }
    }

    // Scan a block for transactions.
    async fn handle_block(&self, peer_id: PeerId, block: Block) -> Option<MainThreadMessage> {
        let chain = self.chain.lock().await;
        let mut block_queue = self.block_queue.lock().await;
        let block_hash = block.block_hash();
        let height = match chain.header_chain.height_of_hash(block_hash) {
            Some(height) => height,
            None => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: "A block received does not have a known hash".into(),
                });
                let mut lock = self.peer_map.lock().await;
                lock.ban(peer_id).await;
                return Some(MainThreadMessage::Disconnect);
            }
        };
        if !block.check_merkle_root() {
            self.dialog.send_warning(Warning::UnexpectedSyncError {
                warning: "A block received does not have a valid merkle root".into(),
            });
            let mut lock = self.peer_map.lock().await;
            lock.ban(peer_id).await;
            return Some(MainThreadMessage::Disconnect);
        }
        let process_block_response = block_queue.process_block(&block_hash);
        match process_block_response {
            ProcessBlockResponse::Accepted { block_recipient } => match block_recipient {
                BlockRecipient::Client(sender) => {
                    let send_err = sender.send(Ok(IndexedBlock::new(height, block))).is_err();
                    if send_err {
                        self.dialog.send_warning(Warning::ChannelDropped);
                    };
                }
                BlockRecipient::Event => {
                    self.dialog
                        .send_event(Event::Block(IndexedBlock::new(height, block)));
                }
            },
            ProcessBlockResponse::LateResponse => {
                crate::log!(
                    self.dialog,
                    format!(
                        "Peer {} responded late to a request for hash {}",
                        peer_id, block_hash
                    )
                );
            }
            ProcessBlockResponse::UnknownHash => {
                crate::log!(
                    self.dialog,
                    format!("Peer {} responded with an irrelevant block", peer_id)
                );
            }
        }
        None
    }

    // The block queue holds all the block hashes we may be interested in
    async fn pop_block_queue(&self) -> Option<MainThreadMessage> {
        let state = self.state.read().await;
        if matches!(
            *state,
            NodeState::FilterHeadersSynced
                | NodeState::FiltersSynced
                | NodeState::TransactionsSynced
        ) {
            let mut queue = self.block_queue.lock().await;
            let next_block_hash = queue.pop();
            return match next_block_hash {
                Some(block_hash) => {
                    crate::log!(self.dialog, format!("Next block in queue: {}", block_hash));
                    Some(MainThreadMessage::GetBlock(block_hash))
                }
                None => None,
            };
        }
        None
    }

    // If new inventory came in, we need to download the headers and update the node state
    async fn handle_inventory_blocks(
        &self,
        nonce: PeerId,
        blocks: Vec<BlockHash>,
    ) -> Option<MainThreadMessage> {
        let mut state = self.state.write().await;
        let mut chain = self.chain.lock().await;
        let mut peer_map = self.peer_map.lock().await;
        for block in blocks.iter() {
            peer_map.increment_height(nonce).await;
            if !chain.header_chain.contains(*block) {
                crate::log!(self.dialog, format!("New block: {}", block));
            }
        }
        match *state {
            NodeState::Behind => None,
            _ => {
                if blocks
                    .into_iter()
                    .any(|block| !chain.header_chain.contains(block))
                {
                    crate::info!(self.dialog, Info::StateChange(NodeState::Behind));
                    *state = NodeState::Behind;
                    let next_headers = GetHeaderConfig {
                        locators: chain.header_chain.locators(),
                        stop_hash: None,
                    };
                    chain.clear_compact_filter_queue();
                    Some(MainThreadMessage::GetHeaders(next_headers))
                } else {
                    None
                }
            }
        }
    }

    // Add more scripts to the chain to look for. Does not imply a rescan.
    async fn add_script(&self, script: ScriptBuf) {
        let mut chain = self.chain.lock().await;
        chain.put_script(script);
    }

    // Clear the filter hash cache and redownload the filters.
    async fn rescan(&self) -> Option<MainThreadMessage> {
        let mut state = self.state.write().await;
        let mut chain = self.chain.lock().await;
        match *state {
            NodeState::Behind => None,
            NodeState::HeadersSynced => None,
            _ => {
                chain.clear_filters();
                crate::info!(
                    self.dialog,
                    Info::StateChange(NodeState::FilterHeadersSynced)
                );
                *state = NodeState::FilterHeadersSynced;
                Some(MainThreadMessage::GetFilters(chain.next_filter_message()))
            }
        }
    }
}
