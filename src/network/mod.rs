use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use addrman::{io::FileExt, Record, Table};
use bitcoin::{
    block::Header,
    consensus::Decodable,
    hashes::Hash,
    io::Read,
    key::rand,
    p2p::{
        address::{AddrV2, AddrV2Message},
        message::CommandString,
        message_blockdata::GetHeadersMessage,
        message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
        message_network::VersionMessage,
        Magic,
    },
    Block, BlockHash, FeeRate, Wtxid,
};
use socks::create_socks5;
use tokio::{net::TcpStream, time::Instant};

use error::PeerError;

pub(crate) mod dns;
pub(crate) mod error;
pub(crate) mod inbound;
pub(crate) mod outbound;
pub(crate) mod peer;
pub(crate) mod peer_map;
pub(crate) mod reader;
pub(crate) mod socks;

pub const PROTOCOL_VERSION: u32 = 70016;
pub const KYOTO_VERSION: &str = "0.3.4";
pub const RUST_BITCOIN_VERSION: &str = "0.32.7";

const THIRTY_MINS: Duration = Duration::from_secs(60 * 30);
const MESSAGE_TIMEOUT_SECS: Duration = Duration::from_secs(5);
//                                            sec  min  hour
const TWO_HOUR: Duration = Duration::from_secs(60 * 60 * 2);
const TCP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
// Ping the peer if we have not exchanged messages for two minutes
const SEND_PING: Duration = Duration::from_secs(60 * 2);
// An absolute maximum timeout to respond to a batch filter request
const MAX_FILTER_RESPONSE_TIME_SEC: Duration = Duration::from_secs(20);

// These are the parameters of the "tried" and "new" tables
const B_TRIED: usize = 4;
const S_TRIED: usize = 16;
const W_TRIED: usize = 2;

const B_NEW: usize = 16;
const S_NEW: usize = 16;
const W_NEW: usize = 8;

// Maximum occurrences of a single network address
const MAX_ADDR: usize = 4;
// How may times a peer can fail before they are terrible
const MAX_ATTEMPS: u8 = 2;
// If it has been less than a week, only allow a single fail
const MAX_WEEKLY_ATTEMPTS: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PeerId(pub(crate) u32);

impl PeerId {
    fn increment(&mut self) {
        self.0 = self.0.wrapping_add(1)
    }
}

impl From<u32> for PeerId {
    fn from(value: u32) -> Self {
        PeerId(value)
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Peer {}", self.0)
    }
}

// Configuration for peer connection timeouts
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub(crate) struct PeerTimeoutConfig {
    // How long to wait for a peer to respond to a request
    pub(crate) response_timeout: Duration,
    // Maximum time to maintain a connection with a peer
    pub(crate) max_connection_time: Duration,
    // How much time does the peer have to make the initial TCP handshake
    pub(crate) handshake_timeout: Duration,
}

impl Default for PeerTimeoutConfig {
    fn default() -> Self {
        Self {
            response_timeout: MESSAGE_TIMEOUT_SECS,
            max_connection_time: TWO_HOUR,
            handshake_timeout: TCP_CONNECTION_TIMEOUT,
        }
    }
}

pub(crate) struct LastBlockMonitor {
    last_block: Option<Instant>,
}

impl LastBlockMonitor {
    pub(crate) fn new() -> Self {
        Self { last_block: None }
    }

    pub(crate) fn reset(&mut self) {
        self.last_block = Some(Instant::now())
    }

    pub(crate) fn stale(&self) -> bool {
        if let Some(time) = self.last_block {
            return time.elapsed() > THIRTY_MINS;
        }
        false
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum ConnectionType {
    #[default]
    ClearNet,
    Socks5Proxy(SocketAddr),
}

impl ConnectionType {
    pub(crate) fn can_connect(&self, addr: &AddrV2) -> bool {
        match &self {
            Self::ClearNet => matches!(addr, AddrV2::Ipv4(_) | AddrV2::Ipv6(_)),
            Self::Socks5Proxy(_) => matches!(addr, AddrV2::Ipv4(_) | AddrV2::Ipv6(_)),
        }
    }

    pub(crate) async fn connect(
        &self,
        addr: AddrV2,
        port: u16,
        handshake_timeout: Duration,
    ) -> Result<TcpStream, PeerError> {
        let socket_addr = match addr {
            AddrV2::Ipv4(ip) => IpAddr::V4(ip),
            AddrV2::Ipv6(ip) => IpAddr::V6(ip),
            _ => return Err(PeerError::UnreachableSocketAddr),
        };
        match &self {
            Self::ClearNet => {
                let timeout = tokio::time::timeout(
                    handshake_timeout,
                    TcpStream::connect((socket_addr, port)),
                )
                .await
                .map_err(|_| PeerError::ConnectionFailed)?;
                let tcp_stream = timeout.map_err(|_| PeerError::ConnectionFailed)?;
                Ok(tcp_stream)
            }
            Self::Socks5Proxy(proxy) => {
                let socks5_timeout = tokio::time::timeout(
                    handshake_timeout,
                    create_socks5(*proxy, socket_addr, port),
                )
                .await
                .map_err(|_| PeerError::ConnectionFailed)?;
                let tcp_stream = socks5_timeout.map_err(PeerError::Socks5)?;
                Ok(tcp_stream)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MessageState {
    general_timeout: Duration,
    version_handshake: VersionHandshakeState,
    verack: VerackState,
    sent_txs: HashSet<Wtxid>,
    timed_message_state: HashMap<TimeSensitiveId, Instant>,
    ping_state: PingState,
    filter_rate: FilterRate,
}

impl MessageState {
    fn new(general_timeout: Duration) -> Self {
        Self {
            general_timeout,
            version_handshake: Default::default(),
            verack: Default::default(),
            sent_txs: Default::default(),
            timed_message_state: Default::default(),
            ping_state: PingState::default(),
            filter_rate: FilterRate::default(),
        }
    }

    fn start_version_handshake(&mut self) {
        self.version_handshake = self.version_handshake.start();
    }

    fn finish_version_handshake(&mut self) {
        self.version_handshake = self.version_handshake.finish();
    }

    fn sent_tx(&mut self, wtxid: Wtxid) {
        self.sent_txs.insert(wtxid);
    }

    fn unknown_rejection(&mut self, wtxid: Wtxid) -> bool {
        !self.sent_txs.remove(&wtxid)
    }

    fn unresponsive(&self) -> bool {
        self.timed_message_state
            .values()
            .any(|time| time.elapsed() > self.general_timeout)
            || self.version_handshake.is_unresponsive(self.general_timeout)
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum VersionHandshakeState {
    #[default]
    NotStarted,
    Started {
        at: tokio::time::Instant,
    },
    Completed,
}

impl VersionHandshakeState {
    fn start(self) -> Self {
        Self::Started {
            at: tokio::time::Instant::now(),
        }
    }

    fn finish(self) -> Self {
        Self::Completed
    }

    fn is_complete(&self) -> bool {
        matches!(self, Self::Completed)
    }

    fn is_unresponsive(&self, timeout: Duration) -> bool {
        match self {
            Self::Started { at } => at.elapsed() > timeout,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct VerackState {
    got_ack: bool,
    sent_ack: bool,
}

impl VerackState {
    fn got_ack(&mut self) {
        self.got_ack = true
    }

    fn sent_ack(&mut self) {
        self.sent_ack = true
    }

    fn both_acks(&self) -> bool {
        self.got_ack && self.sent_ack
    }
}

#[derive(Debug, Clone, Copy)]
enum PingState {
    WaitingFor { nonce: u64 },
    LastMessageReceied { then: Instant },
}

impl PingState {
    fn send_ping(&mut self) -> Option<u64> {
        match self {
            Self::WaitingFor { nonce: _ } => None,
            Self::LastMessageReceied { then } => {
                if then.elapsed() > SEND_PING {
                    let nonce = rand::random();
                    *self = Self::WaitingFor { nonce };
                    Some(nonce)
                } else {
                    None
                }
            }
        }
    }

    fn check_pong(&mut self, pong: u64) -> bool {
        match self {
            Self::WaitingFor { nonce } => {
                if pong.eq(&*nonce) {
                    *self = Self::LastMessageReceied {
                        then: Instant::now(),
                    };
                    true
                } else {
                    false
                }
            }
            Self::LastMessageReceied { then: _ } => false,
        }
    }

    fn update_last_message(&mut self) {
        match self {
            Self::WaitingFor { nonce: _ } => (),
            Self::LastMessageReceied { then: _ } => {
                *self = Self::LastMessageReceied {
                    then: Instant::now(),
                }
            }
        }
    }
}

impl Default for PingState {
    fn default() -> Self {
        Self::LastMessageReceied {
            then: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FilterRate {
    waiting_for: Option<(BlockHash, Instant)>,
}

impl FilterRate {
    fn batch_requested(&mut self, stop_hash: BlockHash) {
        self.waiting_for = Some((stop_hash, Instant::now()))
    }

    fn filter_received(&mut self, block_hash: BlockHash) {
        if let Some((hash, _)) = self.waiting_for {
            if hash.eq(&block_hash) {
                self.waiting_for = None;
            }
        }
    }

    fn slow_peer(&self) -> bool {
        if let Some((_, then)) = self.waiting_for {
            let elapsed = then.elapsed();
            if elapsed > MAX_FILTER_RESPONSE_TIME_SEC {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Copy, std::hash::Hash, PartialEq, Eq, PartialOrd, Ord)]
struct TimeSensitiveId([u8; 32]);

impl TimeSensitiveId {
    const HEADER_MSG: Self = Self([1; 32]);

    const CF_HEADER_MSG: Self = Self([2; 32]);

    const C_FILTER_MSG: Self = Self([3; 32]);

    const PING: Self = Self([4; 32]);

    fn from_slice(slice: [u8; 32]) -> Self {
        Self(slice)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MainThreadMessage {
    GetAddr,
    SendAddrV2,
    WtxidRelay,
    #[allow(unused)]
    SendHeaders,
    GetHeaders(GetHeadersMessage),
    GetFilterHeaders(GetCFHeaders),
    GetFilters(GetCFilters),
    GetBlock(BlockHash),
    Disconnect,
    BroadcastPending,
    Verack,
}

impl MainThreadMessage {
    pub(in crate::network) fn time_sensitive_message_start(
        &self,
    ) -> Option<(TimeSensitiveId, Instant)> {
        match self {
            MainThreadMessage::GetHeaders(_) => Some((TimeSensitiveId::HEADER_MSG, Instant::now())),
            MainThreadMessage::GetFilterHeaders(_) => {
                Some((TimeSensitiveId::CF_HEADER_MSG, Instant::now()))
            }
            MainThreadMessage::GetFilters(_) => {
                Some((TimeSensitiveId::C_FILTER_MSG, Instant::now()))
            }
            MainThreadMessage::GetBlock(hash) => {
                let id = hash.to_raw_hash().to_byte_array();
                Some((TimeSensitiveId::from_slice(id), Instant::now()))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PeerThreadMessage {
    pub nonce: PeerId,
    pub message: PeerMessage,
}

#[derive(Debug)]
pub(crate) enum PeerMessage {
    Version(VersionMessage),
    Headers(Vec<Header>),
    FilterHeaders(CFHeaders),
    Filter(CFilter),
    Block(Block),
    NewBlocks(Vec<BlockHash>),
    FeeFilter(FeeRate),
}

#[derive(Debug)]
struct V1Header {
    magic: Magic,
    _command: CommandString,
    length: u32,
    _checksum: u32,
}

impl Decodable for V1Header {
    fn consensus_decode<R: Read + ?Sized>(
        reader: &mut R,
    ) -> Result<Self, bitcoin::consensus::encode::Error> {
        let magic = Magic::consensus_decode(reader)?;
        let _command = CommandString::consensus_decode(reader)?;
        let length = u32::consensus_decode(reader)?;
        let _checksum = u32::consensus_decode(reader)?;
        Ok(Self {
            magic,
            _command,
            length,
            _checksum,
        })
    }
}

#[derive(Debug)]
pub(crate) struct AddressBook {
    new: Table<B_NEW, S_NEW, W_NEW>,
    tried: Table<B_TRIED, S_TRIED, W_TRIED>,
}

impl AddressBook {
    fn new() -> Self {
        Self {
            new: Table::new(),
            tried: Table::new(),
        }
    }

    pub(crate) fn add_gossiped(
        &mut self,
        gossip: impl Iterator<Item = AddrV2Message>,
        source: &AddrV2,
    ) {
        for addr in gossip {
            let record =
                Record::new_from_addrv2_source(addr.addr, addr.port, addr.services, source);
            if self.new.count(&record) < MAX_ADDR {
                if let Some(conflict) = self.new.add(&record) {
                    if conflict.is_terrible(MAX_ATTEMPS, MAX_WEEKLY_ATTEMPTS) {
                        self.new.remove(&conflict);
                        self.new.add(&record);
                    }
                }
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.new.is_empty() && self.tried.is_empty()
    }

    pub(crate) fn select(&self) -> Option<Record> {
        if self.tried.is_empty() && self.new.is_empty() {
            return None;
        }
        let use_tried: bool = rand::random();
        if use_tried && !self.tried.is_empty() {
            return self.tried.select();
        }
        self.new.select()
    }

    pub(crate) fn failed(&mut self, record: &Record) {
        self.tried.failed_connection(record);
    }

    pub(crate) fn tried(&mut self, record: &Record) {
        self.new.remove(record);
        if let Some(conflict) = self.tried.add(record) {
            self.tried.remove(&conflict);
            self.tried.add(record);
        }
        self.tried.successful_connection(record);
    }

    pub(crate) fn ban(&mut self, record: &Record) {
        self.new.remove(record);
        self.tried.remove(record);
    }

    #[allow(unused)]
    pub(crate) fn write_tables<P: AsRef<PathBuf>>(&self, dir: P) -> Result<(), std::io::Error> {
        let dirname = dir.as_ref();
        let tried_tmp_path = dirname.join("tmp_tried.book");
        let tried_final_path = dirname.join("tried.book");
        let new_tmp_path = dirname.join("tmp_new.book");
        let new_final_path = dirname.join("new.book");
        let mut tried_file = File::create(&tried_tmp_path)?;
        tried_file.write_table(&self.tried)?;
        fs::rename(tried_tmp_path, tried_final_path)?;
        let mut new_file = File::create(&new_tmp_path)?;
        new_file.write_table(&self.new)?;
        fs::rename(new_tmp_path, new_final_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bitcoin::{consensus::deserialize, hashes::Hash, BlockHash, Transaction};

    use crate::network::{LastBlockMonitor, MessageState, PingState};

    use super::FilterRate;

    #[tokio::test(start_paused = true)]
    async fn test_version_message_state() {
        let timeout = Duration::from_secs(1);
        let mut message_state = MessageState::new(timeout);
        assert!(!message_state.unresponsive());
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!message_state.unresponsive());
        message_state.start_version_handshake();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(message_state.unresponsive());
        let mut message_state = MessageState::new(timeout);
        message_state.start_version_handshake();
        message_state.finish_version_handshake();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!message_state.unresponsive());
        assert!(message_state.version_handshake.is_complete());
    }

    #[test]
    fn test_verack_state() {
        let timeout = Duration::from_secs(1);
        let mut messsage_state = MessageState::new(timeout);
        messsage_state.version_handshake.start();
        messsage_state.verack.got_ack();
        assert!(!messsage_state.verack.both_acks());
        messsage_state.verack.sent_ack();
        assert!(messsage_state.verack.both_acks());
    }

    #[test]
    fn test_tx_reject_state() {
        let transaction: Transaction = deserialize(&hex::decode("0200000000010158e87a21b56daf0c23be8e7070456c336f7cbaa5c8757924f545887bb2abdd7501000000171600145f275f436b09a8cc9a2eb2a2f528485c68a56323feffffff02d8231f1b0100000017a914aed962d6654f9a2b36608eb9d64d2b260db4f1118700c2eb0b0000000017a914b7f5faf40e3d40a5a459b1db3535f2b72fa921e88702483045022100a22edcc6e5bc511af4cc4ae0de0fcd75c7e04d8c1c3a8aa9d820ed4b967384ec02200642963597b9b1bc22c75e9f3e117284a962188bf5e8a74c895089046a20ad770121035509a48eb623e10aace8bfd0212fdb8a8e5af3c94b0b133b95e114cab89e4f7965000000").unwrap()).unwrap();
        let wtxid = transaction.compute_wtxid();
        let mut message_state = MessageState::new(Duration::from_secs(2));
        message_state.sent_tx(wtxid);
        assert!(!message_state.unknown_rejection(wtxid));
        assert!(message_state.unknown_rejection(wtxid));
    }

    #[tokio::test(start_paused = true)]
    async fn test_ping_state() {
        // Detect we need a ping
        let mut ping_state = PingState::default();
        assert!(ping_state.send_ping().is_none());
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(ping_state.send_ping().is_none());
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert!(ping_state.send_ping().is_some());
        // Do not spam
        assert!(ping_state.send_ping().is_none());
        // We match pings and update the state correctly
        let mut ping_state = PingState::default();
        tokio::time::sleep(Duration::from_secs(60 * 3)).await;
        let ping = ping_state.send_ping().unwrap();
        tokio::time::sleep(Duration::from_secs(60 * 3)).await;
        assert!(ping_state.check_pong(ping));
        assert!(!ping_state.check_pong(ping));
        assert!(ping_state.send_ping().is_none());
        tokio::time::sleep(Duration::from_secs(60 * 3)).await;
        assert!(ping_state.send_ping().is_some());
        // Receiving a message without a `Pong` does not update the state
        let mut ping_state = PingState::default();
        tokio::time::sleep(Duration::from_secs(60 * 3)).await;
        let ping = ping_state.send_ping().unwrap();
        ping_state.update_last_message();
        assert!(ping_state.check_pong(ping));
        // Time updates properly
        let mut ping_state = PingState::default();
        assert!(ping_state.send_ping().is_none());
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(ping_state.send_ping().is_none());
        ping_state.update_last_message();
        tokio::time::sleep(Duration::from_secs(70)).await;
        assert!(ping_state.send_ping().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_block_detected_stale() {
        let mut last_block = LastBlockMonitor::new();
        tokio::time::sleep(Duration::from_secs(60 * 40)).await;
        // No blocks received yet.
        assert!(!last_block.stale());
        last_block.reset();
        tokio::time::sleep(Duration::from_secs(60 * 20)).await;
        // Has not been thirty minutes
        assert!(!last_block.stale());
        // Should get a block by now
        tokio::time::sleep(Duration::from_secs(60 * 20)).await;
        assert!(last_block.stale());
        last_block.reset();
        assert!(!last_block.stale());
    }

    #[tokio::test(start_paused = true)]
    async fn test_filter_rate_stale() {
        let mut filter_rate = FilterRate::default();
        let block_hash_bytes = [1; 32];
        let block_hash = BlockHash::from_byte_array(block_hash_bytes);
        filter_rate.batch_requested(block_hash);
        assert!(!filter_rate.slow_peer());
        tokio::time::sleep(Duration::from_secs(15)).await;
        assert!(!filter_rate.slow_peer());
        tokio::time::sleep(Duration::from_secs(21)).await;
        assert!(filter_rate.slow_peer());
    }
}
