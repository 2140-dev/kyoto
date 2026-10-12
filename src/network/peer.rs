extern crate tokio;
use std::{sync::Arc, time::Duration};

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
use crate::{broadcaster::BroadcastQueue, messages::Warning, BlockType, Dialog, Info};

use super::{
    error::PeerError,
    inbound::MessageParser,
    outbound::{MessageGenerator, Transport},
    reader::{Reader, ReaderMessage},
    AddressBook, MainThreadMessage, MessageState, PeerId, PeerMessage, PeerThreadMessage,
    PeerTimeoutConfig, TimeSensitiveId,
};

const LOOP_TIMEOUT: Duration = Duration::from_millis(500);
const V2_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

// Filter header requests sent to this peer and not yet answered.
#[derive(Debug, Default, Clone, Copy)]
struct CfHeaderRequests(u32);

impl CfHeaderRequests {
    fn ask(&mut self) {
        self.0 = self.0.saturating_add(1);
    }

    // False for a batch nobody asked this peer for.
    fn answer(&mut self) -> bool {
        match self.0.checked_sub(1) {
            Some(left) => {
                self.0 = left;
                true
            }
            None => false,
        }
    }
}

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
    cf_header_requests: CfHeaderRequests,
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
            cf_header_requests: CfHeaderRequests::default(),
        }
    }

    pub async fn run(
        &mut self,
        connection: TcpStream,
        is_proxy_connection: bool,
    ) -> Result<(), PeerError> {
        let start_time = Instant::now();
        let (tx, mut rx) = mpsc::channel(32);
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
        let read_handle = tokio::spawn(async move { peer_reader.read_from_remote().await });
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
                                        _ => continue,
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
                                        _ => continue,
                                    }
                                },
                            }
                        },
                        None => continue,
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
                if self.message_state.version_handshake.is_complete() {
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
                // Unsolicited batches are dropped, so a peer cannot answer more than it was asked.
                if !self.cf_header_requests.answer() {
                    self.dialog.send_warning(Warning::UnsolicitedMessage);
                    return Ok(());
                }
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
                let mut tx_queue = self.tx_queue.lock().await;
                for inv in requests {
                    match inv {
                        Inventory::WTx(wtxid) => {
                            let transaction = tx_queue.fetch_tx(wtxid);
                            if let Some(transaction) = transaction {
                                let msg = message_generator.broadcast_transaction(transaction);
                                self.write_bytes(writer, msg).await?;
                                self.message_state.sent_tx(wtxid);
                                tx_queue.sent_transaction_payload(wtxid);
                            }
                        }
                        Inventory::Transaction(txid) => {
                            let transaction = tx_queue.fetch_tx(txid);
                            if let Some(transaction) = transaction {
                                let wtxid = transaction.compute_wtxid();
                                let msg = message_generator.broadcast_transaction(transaction);
                                self.write_bytes(writer, msg).await?;
                                self.message_state.sent_tx(wtxid);
                                tx_queue.sent_transaction_payload(wtxid);
                            }
                        }
                        Inventory::WitnessTransaction(txid) => {
                            let transaction = tx_queue.fetch_tx(txid);
                            if let Some(transaction) = transaction {
                                let wtxid = transaction.compute_wtxid();
                                let msg = message_generator.broadcast_transaction(transaction);
                                self.write_bytes(writer, msg).await?;
                                self.message_state.sent_tx(wtxid);
                                tx_queue.sent_transaction_payload(wtxid);
                            }
                        }
                        _ => (),
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
            ReaderMessage::Reject(payload) => {
                if self.message_state.unknown_rejection(payload.wtxid) {
                    self.dialog.send_warning(Warning::UnsolicitedMessage);
                    return Err(PeerError::DisconnectCommand);
                }
                self.dialog
                    .send_warning(Warning::TransactionRejected { payload });
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
            self.message_state.timed_message_state.insert(msg_id, time);
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
                self.cf_header_requests.ask();
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
            MainThreadMessage::GetBlock(message) => {
                let message = message_generator.block(message);
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
        writer.write_all(&message).await?;
        writer.flush().await?;
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
    use super::CfHeaderRequests;

    #[test]
    fn only_requested_filter_headers_are_answers() {
        let mut requests = CfHeaderRequests::default();
        assert!(!requests.answer());
        requests.ask();
        requests.ask();
        assert!(requests.answer());
        assert!(requests.answer());
        assert!(!requests.answer());
    }
}
