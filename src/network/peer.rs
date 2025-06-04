extern crate tokio;
use std::{sync::Arc, time::Duration};

use bip324::{AsyncProtocol, PacketReader, PacketWriter, Role};
use bitcoin::{p2p::ServiceFlags, Network};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    time::Instant,
};

use crate::{
    broadcaster::BroadcastQueue,
    channel_messages::{
        MainThreadMessage, PeerMessage, PeerThreadMessage, ReaderMessage, TimeSensitiveId,
    },
    db::PersistedPeer,
    dialog::Dialog,
    messages::Warning,
    Info, PeerStore,
};

use super::{
    error::PeerError,
    outbound_messages::{MessageGenerator, Transport},
    parsers::MessageParser,
    reader::Reader,
    AddrGossipStages, MessageState, PeerId, PeerTimeoutConfig, TxRequestState,
};

const LOOP_TIMEOUT: Duration = Duration::from_secs(2);
const V2_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);

pub(crate) struct Peer<P: PeerStore + 'static> {
    nonce: PeerId,
    main_thread_sender: Sender<PeerThreadMessage>,
    main_thread_recv: Receiver<MainThreadMessage>,
    network: Network,
    services: ServiceFlags,
    dialog: Arc<Dialog>,
    db: Arc<Mutex<P>>,
    timeout_config: PeerTimeoutConfig,
    message_state: MessageState,
    tx_queue: Arc<Mutex<BroadcastQueue>>,
}

impl<P: PeerStore + 'static> Peer<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        nonce: PeerId,
        network: Network,
        main_thread_sender: Sender<PeerThreadMessage>,
        main_thread_recv: Receiver<MainThreadMessage>,
        services: ServiceFlags,
        dialog: Arc<Dialog>,
        db: Arc<Mutex<P>>,
        timeout_config: PeerTimeoutConfig,
        tx_queue: Arc<Mutex<BroadcastQueue>>,
    ) -> Self {
        Self {
            nonce,
            main_thread_sender,
            main_thread_recv,
            network,
            services,
            dialog,
            db,
            timeout_config,
            message_state: MessageState::new(timeout_config.response_timeout),
            tx_queue,
        }
    }

    pub async fn run(&mut self, connection: TcpStream) -> Result<(), PeerError> {
        let start_time = Instant::now();
        let (tx, mut rx) = mpsc::channel(32);
        let (mut reader, mut writer) = connection.into_split();
        // If a peer signals for V2 we will use it, otherwise just use plaintext.
        let (mut outbound_messages, mut peer_reader) = if self.services.has(ServiceFlags::P2P_V2) {
            let handshake_result = tokio::time::timeout(
                V2_HANDSHAKE_TIMEOUT,
                self.try_handshake(&mut writer, &mut reader),
            )
            .await
            .map_err(|_| PeerError::HandshakeFailed)?;
            if let Err(ref e) = handshake_result {
                crate::log!(
                    self.dialog,
                    format!("Failed to establish an encrypted connection: {e}")
                );
                self.dialog.send_warning(Warning::CouldNotConnect);
            }
            let (decryptor, encryptor) = handshake_result?;
            let outbound_messages = MessageGenerator {
                network: self.network,
                transport: Transport::V2 { encryptor },
            };
            let reader = Reader::new(MessageParser::V2(reader, decryptor), tx);
            (outbound_messages, reader)
        } else {
            let outbound_messages = MessageGenerator {
                network: self.network,
                transport: Transport::V1,
            };
            let reader = Reader::new(MessageParser::V1(reader, self.network), tx);
            (outbound_messages, reader)
        };

        let message = outbound_messages.version_message(None)?;
        self.write_bytes(&mut writer, message).await?;
        self.message_state.start_version_handshake();
        let read_handle = tokio::spawn(async move { peer_reader.read_from_remote().await });
        loop {
            if read_handle.is_finished() {
                return Ok(());
            }
            if self.message_state.addr_state.over_limit() {
                return Ok(());
            }
            if let Some(nonce) = self.message_state.ping_state.send_ping() {
                let msg = outbound_messages.ping(nonce)?;
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
            if Instant::now().duration_since(start_time) > self.timeout_config.max_connection_time {
                crate::log!(self.dialog, format!(
                    "The connection to peer {} has been maintained for over {} seconds, finding a new peer",
                    self.nonce, self.timeout_config.max_connection_time.as_secs(),
                ));
                return Ok(());
            }
            select! {
                // The peer sent us a message
                peer_message = tokio::time::timeout(LOOP_TIMEOUT, rx.recv()) => {
                    if let Ok(peer_message) = peer_message {
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
                match self.message_state.addr_state.gossip_stage {
                    AddrGossipStages::NotReceived => {
                        self.message_state.addr_state.received(addrs.len());
                        let db = Arc::clone(&self.db);
                        let dialog = Arc::clone(&self.dialog);
                        tokio::task::spawn(async move {
                            let mut db = db.lock().await;
                            for peer in addrs {
                                if let Err(e) = db
                                    .update(PersistedPeer::gossiped(
                                        peer.addr,
                                        peer.port,
                                        peer.services,
                                    ))
                                    .await
                                {
                                    dialog.send_warning(Warning::FailedPersistence {
                                        warning: format!(
                                            "Encountered an error adding a gossiped peer: {e}"
                                        ),
                                    });
                                }
                            }
                        });
                        self.message_state.addr_state.first_gossip();
                    }
                    AddrGossipStages::RandomGossip => {
                        self.message_state.addr_state.received(addrs.len());
                        let mut db = self.db.lock().await;
                        for peer in addrs {
                            if let Err(e) = db
                                .update(PersistedPeer::gossiped(
                                    peer.addr,
                                    peer.port,
                                    peer.services,
                                ))
                                .await
                            {
                                self.dialog.send_warning(Warning::FailedPersistence {
                                    warning: format!(
                                        "Encountered an error adding a gossiped peer: {e}"
                                    ),
                                });
                            }
                        }
                    }
                }
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
            ReaderMessage::NewBlocks(block_hashes) => {
                self.main_thread_sender
                    .send(PeerThreadMessage {
                        nonce: self.nonce,
                        message: PeerMessage::NewBlocks(block_hashes),
                    })
                    .await?;
                Ok(())
            }
            ReaderMessage::TxRequests(requests) => {
                let mut tx_queue = self.tx_queue.lock().await;
                for wtxid in requests {
                    let transaction = tx_queue.fetch_tx(wtxid);
                    if let Some(transaction) = transaction {
                        let msg = message_generator.broadcast_transaction(transaction)?;
                        self.write_bytes(writer, msg).await?;
                        self.message_state.sent_tx(wtxid);
                        tx_queue.successful(wtxid);
                        crate::info!(self.dialog, Info::TxGossiped(wtxid))
                    }
                }
                Ok(())
            }
            ReaderMessage::Tx(transaction) => {
                match &mut self.message_state.tx_request_state {
                    TxRequestState::Idle => {
                        crate::log!(self.dialog, "Unexpected transaction message");
                        self.dialog.send_warning(Warning::UnsolicitedMessage);
                        return Err(PeerError::DisconnectCommand);
                    }
                    TxRequestState::Fetching { sender, txids } => {
                        let txid = transaction.compute_txid();
                        let did_request = txids.remove(&txid);
                        if !did_request {
                            crate::log!(self.dialog, "Unexpected transaction message");
                            self.dialog.send_warning(Warning::UnsolicitedMessage);
                            return Err(PeerError::DisconnectCommand);
                        }
                        let _ = sender.send((txid, transaction)).await;
                        if txids.is_empty() {
                            self.message_state.tx_request_state = TxRequestState::Idle;
                        }
                    }
                }
                Ok(())
            }
            ReaderMessage::Verack => {
                if self.message_state.version_handshake.is_complete() {
                    return Err(PeerError::DisconnectCommand);
                }
                self.message_state.finish_version_handshake();
                Ok(())
            }
            ReaderMessage::Ping(nonce) => {
                let message = message_generator.pong(nonce)?;
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
                let message = message_generator.addr()?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetAddrV2 => {
                let message = message_generator.addrv2()?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::WtxidRelay => {
                let message = message_generator.wtxid_relay()?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetHeaders(config) => {
                let message = message_generator.headers(config.locators, config.stop_hash)?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetFilterHeaders(config) => {
                let message = message_generator.cf_headers(config)?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetFilters(config) => {
                let message = message_generator.filters(config)?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::GetBlock(message) => {
                let message = message_generator.block(message)?;
                self.write_bytes(writer, message).await?;
            }
            MainThreadMessage::FetchTransactions(fetch) => {
                let message = message_generator.fetch_txs(fetch.txids.clone())?;
                self.write_bytes(writer, message).await?;
                self.message_state.tx_request_state = TxRequestState::Fetching {
                    sender: fetch.sender,
                    txids: fetch.txids,
                }
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
                    let message = message_generator.announce_transactions(wtxids)?;
                    self.write_bytes(writer, message).await?;
                }
            }
            MainThreadMessage::Verack => {
                let message = message_generator.verack()?;
                self.write_bytes(writer, message).await?;
                crate::info!(self.dialog, Info::SuccessfulHandshake);
                // Take any pending announcements and share them now that the handshake is over
                let wtxids = {
                    let queue = self.tx_queue.lock().await;
                    queue.pending_wtxid()
                };
                if !wtxids.is_empty() {
                    let message = message_generator.announce_transactions(wtxids)?;
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
        writer: &mut W,
        reader: &mut R,
    ) -> Result<(PacketReader, PacketWriter), PeerError>
    where
        W: AsyncWrite + Send + Unpin,
        R: AsyncRead + Send + Unpin,
    {
        crate::log!(
            self.dialog,
            "Initiating a handshake for encrypted messaging"
        );
        let handshake =
            AsyncProtocol::new(self.network, Role::Initiator, None, None, reader, writer).await;
        match handshake {
            Ok(proto) => {
                crate::log!(self.dialog, "Established an encrypted connection");
                let (reader, writer) = proto.into_split();
                Ok((reader.decoder(), writer.encoder()))
            }
            Err(e) => {
                crate::log!(self.dialog, format!("V2 handshake failed {e}"));
                Err(PeerError::HandshakeFailed)
            }
        }
    }
}
