extern crate tokio;
use std::{io, sync::Arc, time::Duration};

use addrman::Record;
use bip324::futures::{Protocol, ProtocolReader};
use bip324::{OutboundCipher, Role};
use bitcoin::BlockHash;
use bitcoin::{
    p2p::{message::NetworkMessage, message_blockdata::Inventory, ServiceFlags},
    Network,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    time::{Instant, MissedTickBehavior},
};

use crate::network::gossip::TransactionCheck;
use crate::network::RelayPolicy;
use crate::{
    broadcaster::{BroadcastQueue, TxIdentifier},
    messages::Warning,
    BlockType, Dialog, Info,
};

use super::{
    error::PeerError,
    inbound::MessageParser,
    outbound::{MessageGenerator, Transport},
    reader::{Reader, ReaderMessage},
    AddressBook, MainThreadMessage, MessageState, PeerId, PeerMessage, PeerThreadMessage,
    PeerTimeoutConfig, TimeSensitiveId,
};

const LOOP_TIMEOUT: Duration = Duration::from_millis(500);
// Writes over Tor may be slow during bootstrap, but we cap the absolute time a peer can be stuck
// during a write. For a reasonable peer, this is a large grace period.
const ABSOLUTE_WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const V2_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

pub(crate) struct Peer {
    nonce: PeerId,
    source: Record,
    main_thread_sender: Sender<PeerThreadMessage>,
    main_thread_recv: Receiver<MainThreadMessage>,
    network: Network,
    block_type: BlockType,
    dialog: Arc<Dialog>,
    db: Arc<Mutex<AddressBook>>,
    timeout_config: PeerTimeoutConfig,
    message_state: MessageState,
    tx_queue: Arc<Mutex<BroadcastQueue>>,
    relay_policy: RelayPolicy,
}

impl Peer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        nonce: PeerId,
        source: Record,
        network: Network,
        block_type: BlockType,
        relay_policy: RelayPolicy,
        main_thread_sender: Sender<PeerThreadMessage>,
        main_thread_recv: Receiver<MainThreadMessage>,
        dialog: Arc<Dialog>,
        db: Arc<Mutex<AddressBook>>,
        timeout_config: PeerTimeoutConfig,
        tx_queue: Arc<Mutex<BroadcastQueue>>,
    ) -> Self {
        Self {
            nonce,
            source,
            main_thread_sender,
            main_thread_recv,
            network,
            block_type,
            relay_policy,
            dialog,
            db,
            timeout_config,
            message_state: MessageState::new(timeout_config.response_timeout),
            tx_queue,
        }
    }

    pub async fn run(
        &mut self,
        connection: TcpStream,
        is_proxy_connection: bool,
    ) -> Result<(), PeerError> {
        let start_time = Instant::now();
        let (tx, mut rx) = mpsc::channel(32);
        let reader_tx = tx.clone();
        let (reader, mut writer) = connection.into_split();
        let reader = BufReader::new(reader);
        // If a peer signals for V2 we will use it, otherwise just use plaintext.
        let (mut outbound_messages, mut peer_reader) =
            if self.source.service_flags().has(ServiceFlags::P2P_V2) && !is_proxy_connection {
                let handshake_result =
                    tokio::time::timeout(V2_HANDSHAKE_TIMEOUT, self.try_handshake(writer, reader))
                        .await
                        .map_err(|_| PeerError::HandshakeFailed)?;
                if handshake_result.is_err() {
                    self.dialog.send_warning(Warning::CouldNotConnect);
                }
                let (protocol_reader, encryptor, w) = handshake_result?;
                writer = w;
                let outbound_messages = MessageGenerator {
                    network: self.network,
                    transport: Transport::V2 { encryptor },
                    block_type: self.block_type,
                };
                let reader = Reader::new(MessageParser::V2(protocol_reader), tx);
                (outbound_messages, reader)
            } else {
                let outbound_messages = MessageGenerator {
                    network: self.network,
                    transport: Transport::V1,
                    block_type: self.block_type,
                };
                let reader = Reader::new(MessageParser::V1(reader, self.network), tx);
                (outbound_messages, reader)
            };

        let message = outbound_messages.version_message(&self.relay_policy);
        self.write_bytes(&mut writer, message).await?;
        self.message_state.start_version_handshake();
        let read_handle = tokio::spawn(async move {
            // Cancel safety: This is not called in a loop, and once `reader_tx` is closed we do
            // not care about parsing messages any longer
            select! {
                result = peer_reader.read_from_remote() => result,
                _ = reader_tx.closed() => Ok(()),
            }
        });
        let mut interval = tokio::time::interval(LOOP_TIMEOUT);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            if read_handle.is_finished() {
                return Ok(());
            }
            if let Some(nonce) = self.message_state.ping_state.send_ping() {
                let msg = outbound_messages.serialize(NetworkMessage::Ping(nonce));
                self.write_bytes(&mut writer, msg).await?;
                let msg_id = TimeSensitiveId::PING;
                self.message_state
                    .timed_message_state
                    .insert(msg_id, Instant::now());
            }
            if self.message_state.unresponsive() {
                self.dialog.send_warning(Warning::PeerTimedOut);
                return Ok(());
            }
            if self.message_state.filter_rate.slow_peer() {
                self.dialog.send_warning(Warning::PeerTimedOut);
                return Ok(());
            }
            if Instant::now().duration_since(start_time) > self.timeout_config.max_connection_time {
                crate::debug!(format!(
                    "The connection to peer {} has been maintained for over {} seconds, finding a new peer",
                    self.nonce, self.timeout_config.max_connection_time.as_secs(),
                ));
                return Ok(());
            }
            select! {
                // The peer sent us a message
                peer_message = rx.recv() => {
                    match peer_message {
                        Some(message) => {
                            match self.handle_peer_message(message, &mut writer, &mut outbound_messages).await {
                                Ok(()) => continue,
                                Err(e) => {
                                    match e {
                                        // We were told by the reader thread to disconnect from this peer
                                        PeerError::DisconnectCommand => return Ok(()),
                                        e => return Err(e),
                                    }
                                },
                            }
                        },
                        None => continue,
                    }
                }
                // The main thread sent us a message
                node_message = self.main_thread_recv.recv() => {
                    match node_message {
                        Some(message) => {
                            match self.main_thread_request(message, &mut writer, &mut outbound_messages).await {
                                Ok(()) => continue,
                                Err(e) => {
                                    match e {
                                        // We were told by the main thread to disconnect from this peer
                                        PeerError::DisconnectCommand => return Ok(()),
                                        e => return Err(e),
                                    }
                                },
                            }
                        },
                        None => return Ok(()),
                    }
                }
                _ = interval.tick() => continue,
            }
        }
    }

    async fn handle_peer_message<W>(
        &mut self,
        message: ReaderMessage,
        writer: &mut W,
        message_generator: &mut MessageGenerator,
    ) -> Result<(), PeerError>
    where
        W: AsyncWrite + Send + Unpin,
    {
        self.message_state.ping_state.update_last_message();
        if let Some(msg_id) = message.time_sensitive_message_received() {
            self.message_state.timed_message_state.remove(&msg_id);
        }
        match message {
            ReaderMessage::Version(version) => {
                if !self.message_state.receive_version() {
                    return Err(PeerError::DisconnectCommand);
                }
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Version(version),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::Addr(addrs) => {
                let mut db_lock = self.db.lock().await;
                db_lock.add_gossiped(addrs.into_iter(), &self.source.network_addr().0);
                Ok(())
            }
            ReaderMessage::Headers(headers) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Headers(headers),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::FilterHeaders(cf_headers) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::FilterHeaders(cf_headers),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::Filter(filter) => {
                self.message_state
                    .filter_rate
                    .filter_received(filter.block_hash);
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Filter(filter),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::Block(block) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::Block(block),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::Inventory(hashes) => match self.relay_policy {
                RelayPolicy::BlocksOnly => {
                    let blocks: Vec<BlockHash> = hashes
                        .into_iter()
                        .filter_map(|inv| match inv {
                            Inventory::Block(hash)
                            | Inventory::CompactBlock(hash)
                            | Inventory::WitnessBlock(hash) => Some(hash),
                            _ => None,
                        })
                        .collect();
                    if blocks.is_empty() {
                        return Ok(());
                    }
                    self.main_thread_sender
                        .send(PeerThreadMessage {
                            nonce: self.nonce,
                            message: PeerMessage::NewBlocks(blocks),
                        })
                        .await?;
                    Ok(())
                }
                RelayPolicy::Transactions(_) => {
                    let requests: Vec<Inventory> = hashes
                        .into_iter()
                        .filter(|inv| {
                            matches!(
                                inv,
                                Inventory::Transaction(_)
                                    | Inventory::WitnessTransaction(_)
                                    | Inventory::WTx(_)
                            )
                        })
                        .collect();
                    if requests.is_empty() {
                        return Ok(());
                    }
                    let msg = message_generator.serialize(NetworkMessage::GetData(requests));
                    self.write_bytes(writer, msg).await?;
                    Ok(())
                }
            },
            ReaderMessage::Transaction(tx) => match &self.relay_policy {
                RelayPolicy::Transactions(watch) => {
                    let watch_guard = watch.lock().await;
                    let tx_check = watch_guard.check_tx(tx);
                    match tx_check {
                        TransactionCheck::SentToClient => {
                            crate::debug!("Found unconfirmed transaction in mempool")
                        }
                        TransactionCheck::Failed => {
                            self.dialog.send_warning(Warning::ChannelDropped);
                        }
                        TransactionCheck::NoMatch => (),
                    }
                    Ok(())
                }
                RelayPolicy::BlocksOnly => Ok(()),
            },
            ReaderMessage::GetData(requests) => {
                for inv in requests {
                    let id: TxIdentifier = match inv {
                        Inventory::WTx(wtxid) => wtxid.into(),
                        Inventory::Transaction(txid) | Inventory::WitnessTransaction(txid) => {
                            txid.into()
                        }
                        _ => continue,
                    };
                    if !self.message_state.was_announced(id) {
                        continue;
                    }
                    let transaction = self.tx_queue.lock().await.fetch_tx(id);
                    if let Some(transaction) = transaction {
                        let wtxid = transaction.compute_wtxid();
                        self.message_state.sent_tx(&transaction);
                        let msg = message_generator.broadcast_transaction(transaction);
                        self.write_bytes(writer, msg).await?;
                        self.tx_queue.lock().await.sent_transaction_payload(wtxid);
                    }
                }
                Ok(())
            }
            ReaderMessage::Verack => {
                if self.message_state.version_handshake.is_complete() {
                    return Err(PeerError::DisconnectCommand);
                }
                // Not expecting two acks
                if self.message_state.verack.got_ack {
                    return Err(PeerError::DisconnectCommand);
                }
                self.message_state.verack.got_ack();
                if self.message_state.verack.both_acks() {
                    self.dialog.send_info(Info::SuccessfulHandshake);
                    self.message_state.finish_version_handshake();
                }
                Ok(())
            }
            ReaderMessage::Ping(nonce) => {
                let message = message_generator.serialize(NetworkMessage::Pong(nonce));
                self.write_bytes(writer, message).await?;
                Ok(())
            }
            ReaderMessage::Pong(nonce) => {
                if self.message_state.ping_state.check_pong(nonce) {
                    Ok(())
                } else {
                    Err(PeerError::DisconnectCommand)
                }
            }
            ReaderMessage::FeeFilter(fee) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::FeeFilter(fee),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::Disconnect => Err(PeerError::DisconnectCommand),
        }
    }

    async fn main_thread_request<W>(
        &mut self,
        request: MainThreadMessage,
        writer: &mut W,
        message_generator: &mut MessageGenerator,
    ) -> Result<(), PeerError>
    where
        W: AsyncWrite + Send + Unpin,
    {
        let time_sensitive = request.time_sensitive_message_start();
        if let Some((msg_id, time)) = time_sensitive {
            self.message_state
                .timed_message_state
                .entry(msg_id)
                .or_insert(time);
        }
        match request {
            MainThreadMessage::GetAddr => {
                let message = message_generator.serialize(NetworkMessage::GetAddr);
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::SendAddrV2 => {
                let message = message_generator.serialize(NetworkMessage::SendAddrV2);
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::WtxidRelay => {
                let message = message_generator.serialize(NetworkMessage::WtxidRelay);
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::SendHeaders => {
                let message = message_generator.serialize(NetworkMessage::SendHeaders);
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetHeaders(config) => {
                let message = message_generator.serialize(NetworkMessage::GetHeaders(config));
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetFilterHeaders(config) => {
                let message = message_generator.serialize(NetworkMessage::GetCFHeaders(config));
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetFilters(config) => {
                self.message_state
                    .filter_rate
                    .batch_requested(config.stop_hash);
                let message = message_generator.serialize(NetworkMessage::GetCFilters(config));
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetBlock(hash, block_type) => {
                let message = message_generator.block(hash, block_type);
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::BroadcastPending => {
                // The peer will drop these on the floor if the handshake is not complete
                if !self.message_state.version_handshake.is_complete() {
                    return Ok(());
                };
                let wtxids = {
                    let queue = self.tx_queue.lock().await;
                    queue.pending_wtxid()
                };
                if !wtxids.is_empty() {
                    self.message_state.announced_txs(&wtxids);
                    let message = message_generator.announce_transactions(wtxids);
                    self.write_bytes(writer, message).await?;
                }
            }
            MainThreadMessage::Verack => {
                let message = message_generator.serialize(NetworkMessage::Verack);
                self.write_bytes(writer, message).await?;
                self.message_state.verack.sent_ack();
                if self.message_state.verack.both_acks() {
                    self.dialog.send_info(Info::SuccessfulHandshake);
                    self.message_state.finish_version_handshake();
                }
                // Take any pending announcements and share them now that the handshake is over
                let wtxids = {
                    let queue = self.tx_queue.lock().await;
                    queue.pending_wtxid()
                };
                if !wtxids.is_empty() {
                    self.message_state.announced_txs(&wtxids);
                    let message = message_generator.announce_transactions(wtxids);
                    self.write_bytes(writer, message).await?;
                }
            }
            MainThreadMessage::Disconnect => return Err(PeerError::DisconnectCommand),
        }
        Ok(())
    }

    async fn write_bytes<W>(&self, writer: &mut W, message: Vec<u8>) -> Result<(), PeerError>
    where
        W: AsyncWrite + Send + Unpin,
    {
        tokio::time::timeout(ABSOLUTE_WRITE_TIMEOUT, async {
            writer.write_all(&message).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        Ok(())
    }

    async fn try_handshake<W, R>(
        &mut self,
        writer: W,
        reader: R,
    ) -> Result<(ProtocolReader<R>, OutboundCipher, W), PeerError>
    where
        W: AsyncWrite + Send + Unpin,
        R: AsyncRead + Send + Unpin,
    {
        crate::debug!("Initiating a handshake for encrypted messaging");
        match Protocol::new(self.network, Role::Initiator, None, None, reader, writer).await {
            Ok(protocol) => {
                crate::debug!("Established an encrypted connection");
                let (protocol_reader, protocol_writer) = protocol.into_split();
                let (outbound_cipher, writer) = protocol_writer.into_inner();
                Ok((protocol_reader, outbound_cipher, writer))
            }
            Err(_) => Err(PeerError::HandshakeFailed),
        }
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        absolute::LockTime,
        consensus::deserialize,
        constants::genesis_block,
        hashes::Hash,
        p2p::{address::AddrV2, message::RawNetworkMessage, message_filter::CFHeaders},
        transaction::Version,
        Amount, FilterHash, FilterHeader, OutPoint, ScriptBuf, Transaction, TxIn, TxOut,
    };

    use super::*;
    use crate::{
        chain::{chain::Chain, error::CFHeaderSyncError, CFHeaderChanges, ChainState},
        FilterType, Package,
    };

    fn transaction_peer(id: u32, tx_queue: Arc<Mutex<BroadcastQueue>>) -> Peer {
        let (to_node, _node_rx) = mpsc::channel(8);
        let (_to_peer, from_node) = mpsc::channel(8);
        let (info_tx, _info_rx) = mpsc::channel(8);
        let (warn_tx, _warn_rx) = mpsc::channel(8);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let address = AddrV2::Ipv4(std::net::Ipv4Addr::LOCALHOST);
        let mut peer = Peer::new(
            PeerId(id),
            Record::new_from_addrv2_source(address.clone(), 18444, ServiceFlags::NONE, &address),
            Network::Regtest,
            BlockType::Witness,
            RelayPolicy::BlocksOnly,
            to_node,
            from_node,
            Arc::new(Dialog::new(info_tx, warn_tx, event_tx)),
            Arc::new(Mutex::new(AddressBook::new())),
            PeerTimeoutConfig::default(),
            tx_queue,
        );
        peer.message_state.finish_version_handshake();
        peer
    }

    async fn get_data(
        peer: &mut Peer,
        generator: &mut MessageGenerator,
        inv: Inventory,
    ) -> Option<Transaction> {
        let mut writer = Vec::new();
        peer.handle_peer_message(ReaderMessage::GetData(vec![inv]), &mut writer, generator)
            .await
            .unwrap();
        if writer.is_empty() {
            return None;
        }
        let message: RawNetworkMessage = deserialize(&writer).unwrap();
        match message.into_payload() {
            NetworkMessage::Tx(transaction) => Some(transaction),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_data_only_serves_announced_transactions() {
        let parent: Transaction = deserialize(&hex::decode("0200000000010158e87a21b56daf0c23be8e7070456c336f7cbaa5c8757924f545887bb2abdd7501000000171600145f275f436b09a8cc9a2eb2a2f528485c68a56323feffffff02d8231f1b0100000017a914aed962d6654f9a2b36608eb9d64d2b260db4f1118700c2eb0b0000000017a914b7f5faf40e3d40a5a459b1db3535f2b72fa921e88702483045022100a22edcc6e5bc511af4cc4ae0de0fcd75c7e04d8c1c3a8aa9d820ed4b967384ec02200642963597b9b1bc22c75e9f3e117284a962188bf5e8a74c895089046a20ad770121035509a48eb623e10aace8bfd0212fdb8a8e5af3c94b0b133b95e114cab89e4f7965000000").unwrap()).unwrap();
        let parent_txid = parent.compute_txid();
        let child = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(parent_txid, 0),
                ..Default::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let child_wtxid = child.compute_wtxid();
        let tx_queue = Arc::new(Mutex::new(BroadcastQueue::new()));
        let (callback, completion) = tokio::sync::oneshot::channel();
        tx_queue.lock().await.add_to_queue(
            Package::new_one_parent_one_child(parent.clone(), child.clone()).unwrap(),
            callback,
        );
        let mut generator = MessageGenerator {
            network: Network::Regtest,
            transport: Transport::V1,
            block_type: BlockType::Witness,
        };
        let mut probing_peer = transaction_peer(1, tx_queue.clone());
        let mut announced_peer = transaction_peer(2, tx_queue);
        announced_peer
            .main_thread_request(
                MainThreadMessage::BroadcastPending,
                &mut tokio::io::sink(),
                &mut generator,
            )
            .await
            .unwrap();

        let parent_inv = Inventory::WitnessTransaction(parent_txid);
        let child_inv = Inventory::WTx(child_wtxid);
        assert!(get_data(&mut probing_peer, &mut generator, child_inv)
            .await
            .is_none());
        assert!(get_data(&mut probing_peer, &mut generator, parent_inv)
            .await
            .is_none());
        assert!(get_data(&mut announced_peer, &mut generator, parent_inv)
            .await
            .is_none());
        assert_eq!(
            get_data(&mut announced_peer, &mut generator, child_inv).await,
            Some(child)
        );
        assert_eq!(
            get_data(&mut announced_peer, &mut generator, parent_inv).await,
            Some(parent)
        );
        assert_eq!(completion.await.unwrap(), child_wtxid);
        assert!(get_data(&mut probing_peer, &mut generator, child_inv)
            .await
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_cfheaders_do_not_clear_pending_request() {
        let network = Network::Regtest;
        let (info_tx, _info_rx) = mpsc::channel(32);
        let (warn_tx, _warn_rx) = mpsc::channel(8);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let dialog = Arc::new(Dialog::new(info_tx, warn_tx, event_tx));
        let mut chain = Chain::new(
            network,
            ChainState::Snapshot(vec![]),
            dialog.clone(),
            1,
            FilterType::Basic,
        );
        let (to_node, mut node_rx) = mpsc::channel(32);
        let (_to_peer, from_node) = mpsc::channel(32);
        let address = AddrV2::Ipv4(std::net::Ipv4Addr::LOCALHOST);
        let services = ServiceFlags::NETWORK | ServiceFlags::COMPACT_FILTERS;
        let mut peer = Peer::new(
            PeerId(1),
            Record::new_from_addrv2_source(address.clone(), 18444, services, &address),
            network,
            BlockType::Legacy,
            RelayPolicy::BlocksOnly,
            to_node,
            from_node,
            dialog,
            Arc::new(Mutex::new(AddressBook::new())),
            PeerTimeoutConfig::default(),
            Arc::new(Mutex::new(BroadcastQueue::new())),
        );
        peer.message_state.finish_version_handshake();
        let mut generator = MessageGenerator {
            network,
            transport: Transport::V1,
            block_type: BlockType::Legacy,
        };
        let mut writer = tokio::io::sink();
        let mut parent = genesis_block(network).header;
        let mut previous_filter_header = FilterHeader::all_zeros();
        let filter_hash = FilterHash::hash(&[0]);
        let mut stale_response = None;

        for height in 1..=3 {
            let mut header = parent;
            header.prev_blockhash = parent.block_hash();
            header.time += 600;
            header.nonce = 0;
            while header.validate_pow(header.target()).is_err() {
                header.nonce += 1;
            }
            chain.sync_chain(vec![header]).unwrap();
            parent = header;
            let request = chain.next_cf_header_message();
            assert_eq!(request.start_height, height);
            peer.main_thread_request(
                MainThreadMessage::GetFilterHeaders(request.clone()),
                &mut writer,
                &mut generator,
            )
            .await
            .unwrap();
            if height == 3 {
                break;
            }
            let response = CFHeaders {
                filter_type: 0,
                stop_hash: request.stop_hash,
                previous_filter_header,
                filter_hashes: vec![filter_hash],
            };
            peer.handle_peer_message(
                ReaderMessage::FilterHeaders(response.clone()),
                &mut writer,
                &mut generator,
            )
            .await
            .unwrap();
            let forwarded = node_rx.recv().await.unwrap();
            let PeerMessage::FilterHeaders(payload) = forwarded.message else {
                panic!("expected filter headers");
            };
            assert!(matches!(
                chain.sync_cf_headers(forwarded.nonce, payload),
                Ok(CFHeaderChanges::Extended)
            ));
            previous_filter_header = filter_hash.filter_header(&previous_filter_header);
            stale_response = Some(response);
        }

        peer.handle_peer_message(
            ReaderMessage::FilterHeaders(stale_response.unwrap()),
            &mut writer,
            &mut generator,
        )
        .await
        .unwrap();
        let forwarded = node_rx.try_recv().unwrap();
        let PeerMessage::FilterHeaders(mut payload) = forwarded.message else {
            panic!("expected filter headers");
        };
        assert!(matches!(
            chain.sync_cf_headers(forwarded.nonce, payload.clone()),
            Ok(CFHeaderChanges::AddedToQueue)
        ));
        assert!(!chain.is_cf_headers_synced());
        payload.stop_hash = parent.block_hash();
        assert!(matches!(
            chain.sync_cf_headers(forwarded.nonce, payload),
            Err(CFHeaderSyncError::PrevHeaderMismatch)
        ));
        tokio::time::advance(peer.timeout_config.response_timeout + Duration::from_secs(1)).await;
        assert!(peer.message_state.unresponsive());
    }
}
