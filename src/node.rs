use std::{sync::Arc, time::Duration};

use bitcoin::{
    block::Header,
    hashes::Hash,
    p2p::{
        message_blockdata::GetHeadersMessage,
        message_filter::{CFHeaders, CFilter},
        message_network::VersionMessage,
        ServiceFlags,
    },
    Block, BlockHash, Network, Wtxid,
};
use tokio::{
    select,
    sync::mpsc::{self},
};
use tokio::{
    sync::{
        mpsc::{Receiver, UnboundedReceiver},
        Mutex,
    },
    time::MissedTickBehavior,
};

use crate::{
    chain::{
        block_queue::{BlockQueue, BlockRecipient, ProcessBlockResponse},
        chain::Chain,
        checkpoints::HeaderCheckpoint,
        error::HeaderSyncError,
        CFHeaderChanges, ChainState, FilterCheck, HeightMonitor,
    },
    error::FetchBlockError,
    messages::ClientRequest,
    network::{
        peer_map::PeerMap, LastBlockMonitor, MainThreadMessage, PeerId, PeerMessage,
        PeerThreadMessage,
    },
    Config, IndexedBlock, NodeState, TxBroadcast, TxBroadcastPolicy,
};

use super::{
    client::Client,
    error::NodeError,
    messages::{ClientMessage, Event, Info, SyncUpdate, Warning},
    Dialog,
};

pub(crate) const WTXID_VERSION: u32 = 70016;
const LOOP_TIMEOUT: Duration = Duration::from_millis(10);

type PeerRequirement = usize;

/// A compact block filter node. Nodes download Bitcoin block headers, block filters, and blocks to send relevant events to a client.
#[derive(Debug)]
pub struct Node {
    state: NodeState,
    chain: Chain,
    peer_map: PeerMap,
    required_peers: PeerRequirement,
    dialog: Arc<Dialog>,
    block_queue: BlockQueue,
    client_recv: UnboundedReceiver<ClientMessage>,
    peer_recv: Receiver<PeerThreadMessage>,
}

impl Node {
    pub(crate) fn new(network: Network, config: Config) -> (Self, Client) {
        let Config {
            required_peers,
            white_list,
            data_path: _,
            chain_state,
            connection_type,
            peer_timeout_config,
        } = config;
        // Set up a communication channel between the node and client
        let (info_tx, info_rx) = mpsc::channel::<Info>(32);
        let (warn_tx, warn_rx) = mpsc::unbounded_channel::<Warning>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
        let (ctx, crx) = mpsc::unbounded_channel::<ClientMessage>();
        let client = Client::new(info_rx, warn_rx, event_rx, ctx);
        // A structured way to talk to the client
        let dialog = Arc::new(Dialog::new(info_tx, warn_tx, event_tx));
        // We always assume we are behind
        let state = NodeState::Behind;
        // Configure the peer manager
        let (mtx, mrx) = mpsc::channel::<PeerThreadMessage>(32);
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let peer_map = PeerMap::new(
            mtx,
            network,
            white_list,
            Arc::clone(&dialog),
            connection_type,
            peer_timeout_config,
            Arc::clone(&height_monitor),
        );
        // Build the chain
        let chain_state = chain_state.unwrap_or(ChainState::Checkpoint(
            HeaderCheckpoint::from_genesis(network),
        ));
        let chain = Chain::new(
            network,
            chain_state,
            Arc::clone(&dialog),
            height_monitor,
            required_peers,
        );
        (
            Self {
                state,
                chain,
                peer_map,
                required_peers: required_peers.into(),
                dialog,
                block_queue: BlockQueue::new(),
                client_recv: crx,
                peer_recv: mrx,
            },
            client,
        )
    }

    /// Run the node continuously. Typically run on a separate thread than the underlying application.
    ///
    /// # Errors
    ///
    /// If the node has exhausted all options to find connections.
    pub async fn run(mut self) -> Result<(), NodeError> {
        crate::debug!("Starting node");
        crate::debug!(format!(
            "Configured connection requirement: {} peers",
            self.required_peers
        ));
        let mut last_block = LastBlockMonitor::new();
        let mut interval = tokio::time::interval(LOOP_TIMEOUT);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            // Try to advance the state of the node
            self.advance_state(&mut last_block).await;
            // Connect to more peers if we need them and remove old connections
            self.dispatch().await?;
            // If there are blocks we need in the queue, we should request them of a random peer
            self.get_blocks().await;
            // Either handle a message from a remote peer or from our client
            select! {
                peer = self.peer_recv.recv() => {
                    match peer {
                        Some(peer_thread) => {
                            match peer_thread.message {
                                PeerMessage::Version(version) => {
                                    self.peer_map.set_services(peer_thread.nonce, version.services);
                                    self.peer_map.set_height(peer_thread.nonce, version.start_height as u32).await;
                                    let response = self.handle_version(peer_thread.nonce, version).await?;
                                    self.peer_map.send_message(peer_thread.nonce, response).await;
                                    crate::debug!(format!("[{}]: version", peer_thread.nonce));
                                }
                                PeerMessage::Headers(headers) => {
                                    last_block.reset();
                                    crate::debug!(format!("[{}]: headers", peer_thread.nonce));
                                    match self.handle_headers(peer_thread.nonce, headers).await {
                                        Some(response) => {
                                            self.peer_map.send_message(peer_thread.nonce, response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::FilterHeaders(cf_headers) => {
                                    crate::debug!(format!("[{}]: filter headers", peer_thread.nonce));
                                    match self.handle_cf_headers(peer_thread.nonce, cf_headers).await {
                                        Some(response) => {
                                            self.peer_map.broadcast(response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::Filter(filter) => {
                                    match self.handle_filter(peer_thread.nonce, filter).await {
                                        Some(response) => {
                                            self.peer_map.send_message(peer_thread.nonce, response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::Block(block) => match self.handle_block(peer_thread.nonce, block).await {
                                    Some(response) => {
                                        self.peer_map.send_message(peer_thread.nonce, response).await;
                                    }
                                    None => continue,
                                },
                                PeerMessage::NewBlocks(blocks) => {
                                    crate::debug!(format!("[{}]: inv", peer_thread.nonce));
                                    match self.handle_inventory_blocks(peer_thread.nonce, blocks).await {
                                        Some(response) => {
                                            self.peer_map.broadcast(response).await;
                                        }
                                        None => continue,
                                    }
                                }
                                PeerMessage::FeeFilter(feerate) => {
                                    self.peer_map.set_broadcast_min(peer_thread.nonce, feerate);
                                }
                            }
                        },
                        _ => continue,
                    }
                },
                message = self.client_recv.recv() => {
                    if let Some(message) = message {
                        match message {
                            ClientMessage::Shutdown => return Ok(()),
                            ClientMessage::Broadcast(transaction) => {
                                self.broadcast_transaction(transaction).await;
                            },
                            ClientMessage::Rescan => {
                                if let Some(response) = self.rescan() {
                                    self.peer_map.broadcast(response).await;
                                }
                            },
                            ClientMessage::GetBlock(request) => {
                                let height_opt = self.chain.header_chain.height_of_hash(request.data());
                                if height_opt.is_none() {
                                    let (_, oneshot) = request.into_values();
                                    let err_reponse = oneshot.send(Err(FetchBlockError::UnknownHash));
                                    if err_reponse.is_err() {
                                        self.dialog.send_warning(Warning::ChannelDropped);
                                    }
                                } else {
                                    crate::debug!(
                                        format!("Adding block {} to queue", request.data())
                                    );
                                    self.block_queue.add(request);
                                }
                            },
                            ClientMessage::AddPeer(peer) => {
                                self.peer_map.add_trusted_peer(peer);
                            },
                            ClientMessage::GetBroadcastMinFeeRate(request) => {
                                let (_, oneshot) = request.into_values();
                                let fee_rate = self.peer_map.broadcast_min();
                                let send_result = oneshot.send(fee_rate);
                                if send_result.is_err() {
                                    self.dialog.send_warning(Warning::ChannelDropped);
                                };
                            }
                            ClientMessage::NoOp => (),
                        }
                    }
                }
                _ = interval.tick() => (),
            }
        }
    }

    // Connect to a new peer if we are not connected to enough
    async fn dispatch(&mut self) -> Result<(), NodeError> {
        self.peer_map.clean().await;
        let live = self.peer_map.live();
        let required = self.next_required_peers();
        // Find more peers when lower than the desired threshold.
        if live < required {
            self.dialog.send_warning(Warning::NeedConnections {
                connected: live,
                required,
            });
            let address = self
                .peer_map
                .next_peer()
                .await
                .ok_or(NodeError::NoReachablePeers)?;
            if self.peer_map.dispatch(address).await.is_err() {
                self.dialog.send_warning(Warning::CouldNotConnect);
            }
        }
        Ok(())
    }

    // If there are blocks in the queue, we should request them of a random peer
    async fn get_blocks(&mut self) {
        if let Some(block_request) = self.pop_block_queue() {
            crate::debug!("Sending block request to random peer");
            self.peer_map.send_random(block_request).await;
        }
    }

    // Broadcast transactions according to the configured policy
    async fn broadcast_transaction(&self, broadcast: ClientRequest<TxBroadcast, Wtxid>) {
        let mut queue = self.peer_map.tx_queue.lock().await;
        let (data, oneshot) = broadcast.into_values();
        let policy = data.broadcast_policy;
        queue.add_to_queue(data.tx, oneshot);
        drop(queue);
        match policy {
            TxBroadcastPolicy::AllPeers => {
                crate::debug!(format!(
                    "Sending transaction to {} connected peers",
                    self.peer_map.live()
                ));
                self.peer_map
                    .broadcast(MainThreadMessage::BroadcastPending)
                    .await
            }
            TxBroadcastPolicy::RandomPeer => {
                crate::debug!("Sending transaction to a random peer");
                self.peer_map
                    .send_random(MainThreadMessage::BroadcastPending)
                    .await
            }
        };
    }

    // Try to continue with the syncing process
    async fn advance_state(&mut self, last_block: &mut LastBlockMonitor) {
        match self.state {
            NodeState::Behind => {
                if self.chain.is_synced().await {
                    self.state = NodeState::HeadersSynced;
                }
            }
            NodeState::HeadersSynced => {
                if self.chain.is_cf_headers_synced() {
                    self.state = NodeState::FilterHeadersSynced;
                }
            }
            NodeState::FilterHeadersSynced => {
                if self.chain.is_filters_synced() {
                    self.state = NodeState::FiltersSynced;
                    let update = SyncUpdate::new(
                        HeaderCheckpoint::new(
                            self.chain.header_chain.height(),
                            self.chain.header_chain.tip_hash(),
                        ),
                        self.chain.last_ten(),
                    );
                    self.dialog.send_event(Event::FiltersSynced(update));
                }
            }
            NodeState::FiltersSynced => {
                if last_block.stale() {
                    self.dialog.send_warning(Warning::PotentialStaleTip);
                    crate::debug!("Disconnecting from remote nodes to find new connections");
                    self.peer_map.broadcast(MainThreadMessage::Disconnect).await;
                    last_block.reset();
                }
            }
        }
    }

    // When syncing headers we are only interested in one peer to start
    fn next_required_peers(&self) -> PeerRequirement {
        match self.state {
            NodeState::Behind => 1,
            _ => self.required_peers,
        }
    }

    // After we receiving some chain-syncing message, we decide what chain of data needs to be
    // requested next.
    async fn next_stateful_message(&mut self) -> Option<MainThreadMessage> {
        if !self.chain.is_synced().await {
            let headers = GetHeadersMessage {
                version: WTXID_VERSION,
                locator_hashes: self.chain.header_chain.locators(),
                stop_hash: BlockHash::all_zeros(),
            };
            return Some(MainThreadMessage::GetHeaders(headers));
        } else if !self.chain.is_cf_headers_synced() {
            return Some(MainThreadMessage::GetFilterHeaders(
                self.chain.next_cf_header_message(),
            ));
        } else if !self.chain.is_filters_synced() {
            return Some(MainThreadMessage::GetFilters(
                self.chain.next_filter_message(),
            ));
        }
        None
    }

    // We accepted a handshake with a peer but we may disconnect if they do not support CBF
    async fn handle_version(
        &mut self,
        nonce: PeerId,
        version_message: VersionMessage,
    ) -> Result<MainThreadMessage, NodeError> {
        if version_message.version < WTXID_VERSION {
            return Ok(MainThreadMessage::Disconnect);
        }
        match self.state {
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
        self.peer_map.tried(nonce).await;
        // First we signal for ADDRV2 support
        self.peer_map
            .send_message(nonce, MainThreadMessage::GetAddrV2)
            .await;
        // Then for BIP 339 witness transaction broadcast
        self.peer_map
            .send_message(nonce, MainThreadMessage::WtxidRelay)
            .await;
        self.peer_map
            .send_message(nonce, MainThreadMessage::Verack)
            .await;
        // Now we may request peers if required
        crate::debug!("Requesting new addresses");
        self.peer_map
            .send_message(nonce, MainThreadMessage::GetAddr)
            .await;
        // Inform the user we are connected to all required peers
        if self.peer_map.live().eq(&self.required_peers) {
            self.dialog.send_info(Info::ConnectionsMet).await;
        }
        // Even if we start the node as caught up in terms of height, we need to check for reorgs. So we can send this unconditionally.
        let next_headers = GetHeadersMessage {
            version: WTXID_VERSION,
            locator_hashes: self.chain.header_chain.locators(),
            stop_hash: BlockHash::all_zeros(),
        };
        Ok(MainThreadMessage::GetHeaders(next_headers))
    }

    // We always send headers to our peers, so our next message depends on our state
    async fn handle_headers(
        &mut self,
        peer_id: PeerId,
        headers: Vec<Header>,
    ) -> Option<MainThreadMessage> {
        let chain = &mut self.chain;
        match chain.sync_chain(headers) {
            Ok(reorgs) => {
                self.chain.send_chain_update().await;
                if !reorgs.is_empty() {
                    self.block_queue.remove(&reorgs);
                }
            }
            Err(e) => match e {
                HeaderSyncError::EmptyMessage => {
                    if !chain.is_synced().await {
                        return Some(MainThreadMessage::Disconnect);
                    }
                    return self.next_stateful_message().await;
                }
                _ => {
                    self.dialog.send_warning(Warning::UnexpectedSyncError {
                        warning: format!("Unexpected header syncing error: {e}"),
                    });
                    self.peer_map.ban(peer_id).await;
                    return Some(MainThreadMessage::Disconnect);
                }
            },
        }
        self.next_stateful_message().await
    }

    // Compact filter headers may result in a number of outcomes, including the need to audit filters.
    async fn handle_cf_headers(
        &mut self,
        peer_id: PeerId,
        cf_headers: CFHeaders,
    ) -> Option<MainThreadMessage> {
        self.chain.send_chain_update().await;
        match self.chain.sync_cf_headers(peer_id, cf_headers) {
            Ok(potential_message) => match potential_message {
                CFHeaderChanges::AddedToQueue => None,
                CFHeaderChanges::Extended => self.next_stateful_message().await,
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
                self.peer_map.ban(peer_id).await;
                Some(MainThreadMessage::Disconnect)
            }
        }
    }

    // Handle a new compact block filter
    async fn handle_filter(
        &mut self,
        peer_id: PeerId,
        filter: CFilter,
    ) -> Option<MainThreadMessage> {
        match self.chain.sync_filter(filter) {
            Ok(potential_message) => {
                let FilterCheck { was_last_in_batch } = potential_message;
                if was_last_in_batch {
                    self.chain.send_chain_update().await;
                    if !self.chain.is_filters_synced() {
                        let next_filters = self.chain.next_filter_message();
                        return Some(MainThreadMessage::GetFilters(next_filters));
                    }
                }
                None
            }
            Err(e) => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: format!("Compact filter syncing encountered an error: {e}"),
                });
                self.peer_map.ban(peer_id).await;
                Some(MainThreadMessage::Disconnect)
            }
        }
    }

    // Scan a block for transactions.
    async fn handle_block(&mut self, peer_id: PeerId, block: Block) -> Option<MainThreadMessage> {
        let block_hash = block.block_hash();
        let height = match self.chain.header_chain.height_of_hash(block_hash) {
            Some(height) => height,
            None => {
                self.dialog.send_warning(Warning::UnexpectedSyncError {
                    warning: "A block received does not have a known hash".into(),
                });
                self.peer_map.ban(peer_id).await;
                return Some(MainThreadMessage::Disconnect);
            }
        };
        if !block.check_merkle_root() {
            self.dialog.send_warning(Warning::UnexpectedSyncError {
                warning: "A block received does not have a valid merkle root".into(),
            });
            self.peer_map.ban(peer_id).await;
            return Some(MainThreadMessage::Disconnect);
        }
        let process_block_response = self.block_queue.process_block(&block_hash);
        match process_block_response {
            ProcessBlockResponse::Accepted { block_recipient } => {
                self.dialog
                    .send_info(Info::BlockReceived(block.block_hash()))
                    .await;
                match block_recipient {
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
                }
            }
            ProcessBlockResponse::LateResponse => {
                crate::debug!(format!(
                    "Peer {} responded late to a request for hash {}",
                    peer_id, block_hash
                ));
            }
            ProcessBlockResponse::UnknownHash => {
                crate::debug!(format!(
                    "Peer {} responded with an irrelevant block",
                    peer_id
                ));
            }
        }
        None
    }

    // The block queue holds all the block hashes we may be interested in
    fn pop_block_queue(&mut self) -> Option<MainThreadMessage> {
        if matches!(
            self.state,
            NodeState::FilterHeadersSynced | NodeState::FiltersSynced
        ) {
            let next_block_hash = self.block_queue.pop();
            return next_block_hash.map(MainThreadMessage::GetBlock);
        }
        None
    }

    // If new inventory came in, we need to download the headers and update the node state
    async fn handle_inventory_blocks(
        &mut self,
        nonce: PeerId,
        blocks: Vec<BlockHash>,
    ) -> Option<MainThreadMessage> {
        for block in blocks.iter() {
            if !self.chain.header_chain.contains(*block) {
                self.peer_map.increment_height(nonce).await;
                crate::debug!(format!("New block: {}", block));
            }
        }
        match self.state {
            NodeState::Behind => None,
            _ => {
                if blocks
                    .into_iter()
                    .any(|block| !self.chain.header_chain.contains(block))
                {
                    self.state = NodeState::Behind;
                    let next_headers = GetHeadersMessage {
                        version: WTXID_VERSION,
                        locator_hashes: self.chain.header_chain.locators(),
                        stop_hash: BlockHash::all_zeros(),
                    };
                    self.chain.clear_compact_filter_queue();
                    Some(MainThreadMessage::GetHeaders(next_headers))
                } else {
                    None
                }
            }
        }
    }

    // Clear the filter hash cache and redownload the filters.
    fn rescan(&mut self) -> Option<MainThreadMessage> {
        match self.state {
            NodeState::Behind => None,
            NodeState::HeadersSynced => None,
            _ => {
                self.chain.clear_filters();
                self.state = NodeState::FilterHeadersSynced;
                Some(MainThreadMessage::GetFilters(
                    self.chain.next_filter_message(),
                ))
            }
        }
    }
}
