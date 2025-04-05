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
use tokio::sync::{mpsc::Receiver, Mutex, RwLock};
use tokio::{
    select,
    sync::mpsc::{self},
};

use crate::{
    chain::{
        chain::Chain,
        checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
        error::HeaderSyncError,
        HeightMonitor,
    },
    db::traits::{HeaderStore, PeerStore},
    error::FetchHeaderError,
    filters::cfheader_chain::AppendAttempt,
    network::{peer_map::PeerMap, LastBlockMonitor, PeerId, PeerTimeoutConfig},
    FilterSyncPolicy, NodeState, RejectPayload, TxBroadcastPolicy,
};

use super::{
    broadcaster::Broadcaster,
    channel_messages::{
        CombinedAddr, GetBlockConfig, GetHeaderConfig, MainThreadMessage, PeerMessage,
        PeerThreadMessage,
    },
    client::Client,
    config::NodeConfig,
    dialog::Dialog,
    error::NodeError,
    messages::{ClientMessage, Event, Log, SyncUpdate, Warning},
};

pub(crate) const WTXID_VERSION: u32 = 70016;
const LOOP_TIMEOUT: u64 = 1;

type PeerRequirement = usize;

/// A compact block filter node. Nodes download Bitcoin block headers, block filters, and blocks to send relevant events to a client.
#[derive(Debug)]
pub struct Node<H: HeaderStore, P: PeerStore> {
    state: Arc<RwLock<NodeState>>,
    chain: Arc<Mutex<Chain<H>>>,
    peer_map: Arc<Mutex<PeerMap<P>>>,
    tx_broadcaster: Arc<Mutex<Broadcaster>>,
    required_peers: PeerRequirement,
    dialog: Arc<Dialog>,
    client_recv: Arc<Mutex<Receiver<ClientMessage>>>,
    peer_recv: Arc<Mutex<Receiver<PeerThreadMessage>>>,
    filter_sync_policy: Arc<RwLock<FilterSyncPolicy>>,
}

impl<H: HeaderStore, P: PeerStore> Node<H, P> {
    pub(crate) fn new(
        network: Network,
        config: NodeConfig,
        peer_store: P,
        header_store: H,
    ) -> (Self, Client) {
        let NodeConfig {
            required_peers,
            white_list,
            dns_resolver,
            addresses,
            data_path: _,
            header_checkpoint,
            connection_type,
            target_peer_size,
            response_timeout,
            max_connection_time,
            filter_sync_policy,
            log_level,
        } = config;
        let timeout_config = PeerTimeoutConfig::new(response_timeout, max_connection_time);
        // Set up a communication channel between the node and client
        let (log_tx, log_rx) = mpsc::channel::<Log>(32);
        let (warn_tx, warn_rx) = mpsc::unbounded_channel::<Warning>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (ctx, crx) = mpsc::channel::<ClientMessage>(5);
        let client = Client::new(log_rx, warn_rx, event_rx, ctx);
        // A structured way to talk to the client
        let dialog = Arc::new(Dialog::new(log_level, log_tx, warn_tx, event_tx));
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
            timeout_config,
            Arc::clone(&height_monitor),
            dns_resolver,
        )));
        // Set up the transaction broadcaster
        let tx_broadcaster = Arc::new(Mutex::new(Broadcaster::new()));
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
            header_store,
            required_peers.into(),
        );
        let chain = Arc::new(Mutex::new(chain));
        (
            Self {
                state,
                chain,
                peer_map,
                tx_broadcaster,
                required_peers: required_peers.into(),
                dialog,
                client_recv: Arc::new(Mutex::new(crx)),
                peer_recv: Arc::new(Mutex::new(mrx)),
                filter_sync_policy: Arc::new(RwLock::new(filter_sync_policy)),
            },
            client,
        )
    }

    /// Run the node continuously. Typically run on a separate thread than the underlying application.
    ///
    /// # Errors
    ///
    /// A node will cease running if a fatal error is encountered with either the [`PeerStore`] or [`HeaderStore`].
    pub async fn run(&self) -> Result<(), NodeError<H::Error, P::Error>> {
        crate::log!(self.dialog, "Starting node");
        crate::log!(
            self.dialog,
            format!(
                "Configured connection requirement: {} peers",
                self.required_peers
            )
        );
        self.fetch_headers().await?;
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
            // If we have a transaction to broadcast and we are connected to peers, we should broadcast them
            self.broadcast_transactions().await;
            // Either handle a message from a remote peer or from our client
            select! {
                peer = tokio::time::timeout(Duration::from_secs(LOOP_TIMEOUT), peer_recv.recv()) => {
                    match peer {
                        Ok(Some(peer_thread)) => {
                            match peer_thread.message {
                                PeerMessage::Version(version) => {
                                    {
                                        let mut peer_map = self.peer_map.lock().await;
                                        peer_map.set_offset(peer_thread.nonce, version.timestamp);
                                        peer_map.set_services(peer_thread.nonce, version.services);
                                        peer_map.set_height(peer_thread.nonce, version.start_height as u32).await;
                                    }
                                    let response = self.handle_version(peer_thread.nonce, version).await?;
                                    self.send_message(peer_thread.nonce, response).await;
                                    crate::log!(self.dialog, format!("[{}]: version", peer_thread.nonce));
                                }
                                PeerMessage::Addr(addresses) => self.handle_new_addrs(addresses).await,
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
                                PeerMessage::Reject(payload) => {
                                    self.dialog
                                        .send_warning(Warning::TransactionRejected { payload });
                                }
                                PeerMessage::FeeFilter(feerate) => {
                                    let mut peer_map = self.peer_map.lock().await;
                                    peer_map.set_broadcast_min(peer_thread.nonce, feerate);
                                }
                                _ => continue,
                            }
                        },
                        _ => continue,
                    }
                },
                message = client_recv.recv() => {
                    if let Some(message) = message {
                        match message {
                            ClientMessage::Shutdown => return Ok(()),
                            ClientMessage::Broadcast(transaction) => self.tx_broadcaster.lock().await.add(transaction),
                            ClientMessage::AddScript(script) =>  self.add_script(script).await,
                            ClientMessage::Rescan => {
                                if let Some(response) = self.rescan().await {
                                    self.broadcast(response).await;
                                }
                            },
                            ClientMessage::ContinueDownload => {
                                if let Some(response) = self.start_filter_download().await {
                                    self.broadcast(response).await
                                }
                            },
                            #[cfg(feature = "filter-control")]
                            ClientMessage::GetBlock(hash) => {
                                let mut state = self.state.write().await;
                                if matches!(*state, NodeState::TransactionsSynced) {
                                    *state = NodeState::FiltersSynced
                                }
                                drop(state);
                                let mut chain = self.chain.lock().await;
                                chain.get_block(hash).await;
                            },
                            ClientMessage::SetDuration(duration) => {
                                let mut peer_map = self.peer_map.lock().await;
                                peer_map.set_duration(duration);
                            },
                            ClientMessage::AddPeer(peer) => {
                                let mut peer_map = self.peer_map.lock().await;
                                peer_map.add_trusted_peer(peer);
                            },
                            ClientMessage::GetHeader(request) => {
                                let mut chain = self.chain.lock().await;
                                let header_opt = chain.fetch_header(request.height).await.map_err(|e| FetchHeaderError::DatabaseOptFailed { error: e.to_string() }).and_then(|opt| opt.ok_or(FetchHeaderError::UnknownHeight));
                                let send_result = request.oneshot.send(header_opt);
                                if send_result.is_err() {
                                    self.dialog.send_warning(Warning::ChannelDropped);
                                };
                            },
                            ClientMessage::GetHeaderBatch(request) => {
                                let chain = self.chain.lock().await;
                                let range_opt = chain.fetch_header_range(request.range).await.map_err(|e| FetchHeaderError::DatabaseOptFailed { error: e.to_string() });
                                let send_result = request.oneshot.send(range_opt);
                                if send_result.is_err() {
                                    self.dialog.send_warning(Warning::ChannelDropped);
                                };
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

    // Broadcast a messsage to all connected peers
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
    async fn dispatch(&self) -> Result<(), NodeError<H::Error, P::Error>> {
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
    async fn broadcast_transactions(&self) {
        let mut broadcaster = self.tx_broadcaster.lock().await;
        if broadcaster.is_empty() {
            return;
        }
        let mut peer_map = self.peer_map.lock().await;
        if peer_map.live().ge(&self.required_peers) {
            for transaction in broadcaster.queue() {
                let txid = transaction.tx.compute_txid();
                let did_broadcast = match transaction.broadcast_policy {
                    TxBroadcastPolicy::AllPeers => {
                        crate::log!(
                            self.dialog,
                            format!("Sending transaction to {} connected peers", peer_map.live())
                        );
                        peer_map
                            .broadcast(MainThreadMessage::BroadcastTx(transaction.tx))
                            .await
                    }
                    TxBroadcastPolicy::RandomPeer => {
                        crate::log!(self.dialog, "Sending transaction to a random peer");
                        peer_map
                            .send_random(MainThreadMessage::BroadcastTx(transaction.tx))
                            .await
                    }
                };
                if did_broadcast {
                    self.dialog.send_info(Log::TxSent(txid)).await;
                } else {
                    self.dialog.send_warning(Warning::TransactionRejected {
                        payload: RejectPayload::from_txid(txid),
                    });
                }
            }
        }
    }

    // Try to continue with the syncing process
    async fn advance_state(&self, last_block: &mut LastBlockMonitor) {
        let mut state = self.state.write().await;
        match *state {
            NodeState::Behind => {
                let mut header_chain = self.chain.lock().await;
                if header_chain.is_synced().await {
                    header_chain.flush_to_disk().await;
                    self.dialog
                        .send_info(Log::StateChange(NodeState::HeadersSynced))
                        .await;
                    *state = NodeState::HeadersSynced;
                }
            }
            NodeState::HeadersSynced => {
                let header_chain = self.chain.lock().await;
                if header_chain.is_cf_headers_synced() {
                    self.dialog
                        .send_info(Log::StateChange(NodeState::FilterHeadersSynced))
                        .await;
                    *state = NodeState::FilterHeadersSynced;
                }
            }
            NodeState::FilterHeadersSynced => {
                let header_chain = self.chain.lock().await;
                if header_chain.is_filters_synced() {
                    self.dialog
                        .send_info(Log::StateChange(NodeState::FiltersSynced))
                        .await;
                    *state = NodeState::FiltersSynced;
                }
            }
            NodeState::FiltersSynced => {
                let header_chain = self.chain.lock().await;
                if header_chain.block_queue_empty() {
                    *state = NodeState::TransactionsSynced;
                    let update = SyncUpdate::new(
                        HeaderCheckpoint::new(header_chain.height(), header_chain.tip()),
                        header_chain.last_ten(),
                    );
                    self.dialog
                        .send_info(Log::StateChange(NodeState::TransactionsSynced))
                        .await;
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
    async fn next_stateful_message(&self, chain: &mut Chain<H>) -> Option<MainThreadMessage> {
        if !chain.is_synced().await {
            let headers = GetHeaderConfig {
                locators: chain.locators().await,
                stop_hash: None,
            };
            return Some(MainThreadMessage::GetHeaders(headers));
        } else if !chain.is_cf_headers_synced() {
            return Some(MainThreadMessage::GetFilterHeaders(
                chain.next_cf_header_message().await,
            ));
        } else if !chain.is_filters_synced() {
            let filter_download = self.filter_sync_policy.read().await;
            if matches!(*filter_download, FilterSyncPolicy::Continue) {
                return Some(MainThreadMessage::GetFilters(
                    chain.next_filter_message().await,
                ));
            }
        }
        None
    }

    // We accepted a handshake with a peer but we may disconnect if they do not support CBF
    async fn handle_version(
        &self,
        nonce: PeerId,
        version_message: VersionMessage,
    ) -> Result<MainThreadMessage, NodeError<H::Error, P::Error>> {
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
            self.dialog.send_info(Log::ConnectionsMet).await
        }
        // Even if we start the node as caught up in terms of height, we need to check for reorgs. So we can send this unconditionally.
        let mut chain = self.chain.lock().await;
        let next_headers = GetHeaderConfig {
            locators: chain.locators().await,
            stop_hash: None,
        };
        Ok(MainThreadMessage::GetHeaders(next_headers))
    }

    // Handle new addresses gossiped over the p2p network
    async fn handle_new_addrs(&self, new_peers: Vec<CombinedAddr>) {
        crate::log!(
            self.dialog,
            format!("Adding {} new peers to the peer database", new_peers.len())
        );
        let mut peer_map = self.peer_map.lock().await;
        peer_map.add_gossiped_peers(new_peers).await;
    }

    // We always send headers to our peers, so our next message depends on our state
    async fn handle_headers(
        &self,
        peer_id: PeerId,
        headers: Vec<Header>,
    ) -> Option<MainThreadMessage> {
        let mut chain = self.chain.lock().await;
        if let Err(e) = chain.sync_chain(headers).await {
            match e {
                HeaderSyncError::EmptyMessage => {
                    if !chain.is_synced().await {
                        return Some(MainThreadMessage::Disconnect);
                    }
                    return self.next_stateful_message(chain.deref_mut()).await;
                }
                HeaderSyncError::LessWorkFork => {
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: "A peer sent us a fork with less work.".into(),
                    });
                    return Some(MainThreadMessage::Disconnect);
                }
                _ => {
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: format!("Unexpected header syncing error: {}", e),
                    });
                    let mut lock = self.peer_map.lock().await;
                    lock.ban(peer_id).await;
                    return Some(MainThreadMessage::Disconnect);
                }
            }
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
        match chain.sync_cf_headers(peer_id.0, cf_headers).await {
            Ok(potential_message) => match potential_message {
                AppendAttempt::AddedToQueue => None,
                AppendAttempt::Extended => self.next_stateful_message(chain.deref_mut()).await,
                AppendAttempt::Conflict(_) => {
                    // TODO: Request the filter and block from the peer
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: "Found a conflict while peers are sending filter headers".into(),
                    });
                    Some(MainThreadMessage::Disconnect)
                }
            },
            Err(e) => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: format!("Compact filter header syncing encountered an error: {}", e),
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
        match chain.sync_filter(filter).await {
            Ok(potential_message) => potential_message.map(MainThreadMessage::GetFilters),
            Err(e) => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: format!("Compact filter syncing encountered an error: {}", e),
                });
                let mut lock = self.peer_map.lock().await;
                lock.ban(peer_id).await;
                Some(MainThreadMessage::Disconnect)
            }
        }
    }

    // Scan a block for transactions.
    async fn handle_block(&self, peer_id: PeerId, block: Block) -> Option<MainThreadMessage> {
        let mut chain = self.chain.lock().await;
        if let Err(e) = chain.check_send_block(block).await {
            self.dialog.send_warning(Warning::UnexpectedSyncError {
                warning: format!("Unexpected block scanning error: {}", e),
            });
            let mut lock = self.peer_map.lock().await;
            lock.ban(peer_id).await;
            return Some(MainThreadMessage::Disconnect);
        }
        None
    }

    // The block queue holds all the block hashes we may be interested in
    async fn pop_block_queue(&self) -> Option<MainThreadMessage> {
        let state = self.state.read().await;
        if matches!(
            *state,
            NodeState::FilterHeadersSynced | NodeState::FiltersSynced
        ) {
            let mut chain = self.chain.lock().await;
            let next_block_hash = chain.next_block();
            return match next_block_hash {
                Some(block_hash) => {
                    crate::log!(self.dialog, format!("Next block in queue: {}", block_hash));
                    Some(MainThreadMessage::GetBlock(GetBlockConfig {
                        locator: block_hash,
                    }))
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
            if !chain.contains_hash(*block) {
                crate::log!(self.dialog, format!("New block: {}", block));
            }
        }
        match *state {
            NodeState::Behind => None,
            _ => {
                if blocks.into_iter().any(|block| !chain.contains_hash(block)) {
                    self.dialog
                        .send_info(Log::StateChange(NodeState::Behind))
                        .await;
                    *state = NodeState::Behind;
                    let next_headers = GetHeaderConfig {
                        locators: chain.locators().await,
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
                self.dialog
                    .send_info(Log::StateChange(NodeState::FilterHeadersSynced))
                    .await;
                *state = NodeState::FilterHeadersSynced;
                Some(MainThreadMessage::GetFilters(
                    chain.next_filter_message().await,
                ))
            }
        }
    }

    // Continue the filter syncing process by explicit command
    async fn start_filter_download(&self) -> Option<MainThreadMessage> {
        let mut download_policy = self.filter_sync_policy.write().await;
        *download_policy = FilterSyncPolicy::Continue;
        drop(download_policy);
        let current_state = self.state.read().await;
        match *current_state {
            NodeState::Behind => None,
            NodeState::HeadersSynced => None,
            _ => {
                let mut chain = self.chain.lock().await;
                self.next_stateful_message(chain.deref_mut()).await
            }
        }
    }

    // When the application starts, fetch any headers we know about from the database.
    async fn fetch_headers(&self) -> Result<(), NodeError<H::Error, P::Error>> {
        crate::log!(self.dialog, "Attempting to load headers from the database");
        let mut chain = self.chain.lock().await;
        chain
            .load_headers()
            .await
            .map_err(NodeError::HeaderDatabase)
    }
}
