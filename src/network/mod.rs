use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use bitcoin::{
    consensus::Decodable,
    io::Read,
    key::rand,
    p2p::{address::AddrV2, message::CommandString, Magic},
    Transaction, Txid, Wtxid,
};
use socks::create_socks5;
use tokio::sync::mpsc;
use tokio::{net::TcpStream, time::Instant};

use error::PeerError;

use crate::channel_messages::TimeSensitiveId;

pub(crate) mod dns;
pub(crate) mod error;
pub(crate) mod outbound_messages;
pub(crate) mod parsers;
pub(crate) mod peer;
pub(crate) mod peer_map;
pub(crate) mod reader;
pub(crate) mod socks;

pub const PROTOCOL_VERSION: u32 = 70016;
pub const KYOTO_VERSION: &str = "0.12.1";
pub const RUST_BITCOIN_VERSION: &str = "0.32.6";

const THIRTY_MINS: Duration = Duration::from_secs(60 * 30);
const MESSAGE_TIMEOUT_SECS: Duration = Duration::from_secs(5);
//                                            sec  min  hour
const TWO_HOUR: Duration = Duration::from_secs(60 * 60 * 2);
const TCP_CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
// Ping the peer if we have not exchanged messages for two minutes
const SEND_PING: Duration = Duration::from_secs(60 * 2);

// A peer cannot send 10,000 ADDRs in one connection.
const ADDR_HARD_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PeerId(pub(crate) u32);

impl PeerId {
    pub(crate) fn increment(&mut self) {
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

/// Configuration for peer connection timeouts
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PeerTimeoutConfig {
    /// How long to wait for a peer to respond to a request
    pub(crate) response_timeout: Duration,
    /// Maximum time to maintain a connection with a peer
    pub(crate) max_connection_time: Duration,
    /// How much time does the peer have to make the initial TCP handshake
    pub(crate) handshake_timeout: Duration,
}

impl PeerTimeoutConfig {
    /// Create a new peer timeout configuration
    pub fn new(
        response_timeout: Duration,
        max_connection_time: Duration,
        handshake_timeout: Duration,
    ) -> Self {
        Self {
            response_timeout,
            max_connection_time,
            handshake_timeout,
        }
    }
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
            return Instant::now().duration_since(time) > THIRTY_MINS;
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
    addr_state: AddrGossipState,
    sent_txs: HashSet<Wtxid>,
    timed_message_state: HashMap<TimeSensitiveId, Instant>,
    ping_state: PingState,
    tx_request_state: TxRequestState,
}

impl MessageState {
    fn new(general_timeout: Duration) -> Self {
        Self {
            general_timeout,
            version_handshake: Default::default(),
            addr_state: Default::default(),
            sent_txs: Default::default(),
            timed_message_state: Default::default(),
            ping_state: PingState::default(),
            tx_request_state: TxRequestState::default(),
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
struct AddrGossipState {
    num_advertised: usize,
    gossip_stage: AddrGossipStages,
}

impl AddrGossipState {
    fn received(&mut self, num_addrs: usize) {
        self.num_advertised += num_addrs;
    }

    fn first_gossip(&mut self) {
        self.gossip_stage = AddrGossipStages::RandomGossip;
    }

    fn over_limit(&self) -> bool {
        self.num_advertised > ADDR_HARD_LIMIT
    }
}

// Network address gossip occurs in multiple stages. First, we will send a `getaddr` message to
// inform the peer that we want to know about nodes they are aware of. Oftentimes this will result
// in a message containing 250-300 potential peers. Thereafter, the remote node will randomly send
// 1-5 potential peers throughout the duration of the connection.
#[derive(Debug, Clone, Copy, Default)]
enum AddrGossipStages {
    #[default]
    NotReceived,
    RandomGossip,
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
                if Instant::now().duration_since(*then) > SEND_PING {
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
enum TxRequestState {
    #[default]
    Idle,
    Fetching {
        sender: mpsc::Sender<(Txid, Transaction)>,
        txids: HashSet<Txid>,
    },
}

pub(crate) struct V1Header {
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

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, time::Duration};

    use bitcoin::{consensus::deserialize, p2p::address::AddrV2, Transaction};

    use crate::{
        network::{AddrGossipStages, MessageState, PingState},
        prelude::Netgroup,
    };

    #[test]
    fn test_sixteen() {
        let peer = AddrV2::Ipv4(Ipv4Addr::new(95, 217, 198, 121));
        assert_eq!("95.217".to_string(), peer.netgroup());
    }

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
    fn test_tx_reject_state() {
        let transaction: Transaction = deserialize(&hex::decode("0200000000010158e87a21b56daf0c23be8e7070456c336f7cbaa5c8757924f545887bb2abdd7501000000171600145f275f436b09a8cc9a2eb2a2f528485c68a56323feffffff02d8231f1b0100000017a914aed962d6654f9a2b36608eb9d64d2b260db4f1118700c2eb0b0000000017a914b7f5faf40e3d40a5a459b1db3535f2b72fa921e88702483045022100a22edcc6e5bc511af4cc4ae0de0fcd75c7e04d8c1c3a8aa9d820ed4b967384ec02200642963597b9b1bc22c75e9f3e117284a962188bf5e8a74c895089046a20ad770121035509a48eb623e10aace8bfd0212fdb8a8e5af3c94b0b133b95e114cab89e4f7965000000").unwrap()).unwrap();
        let wtxid = transaction.compute_wtxid();
        let mut message_state = MessageState::new(Duration::from_secs(2));
        message_state.sent_tx(wtxid);
        assert!(!message_state.unknown_rejection(wtxid));
        assert!(message_state.unknown_rejection(wtxid));
    }

    #[test]
    fn test_addr_gossip_state() {
        let mut message_state = MessageState::new(Duration::from_secs(2));
        assert!(matches!(
            message_state.addr_state.gossip_stage,
            AddrGossipStages::NotReceived
        ));
        message_state.addr_state.received(100);
        message_state.addr_state.first_gossip();
        assert!(matches!(
            message_state.addr_state.gossip_stage,
            AddrGossipStages::RandomGossip
        ));
        assert!(!message_state.addr_state.over_limit());
        message_state.addr_state.received(10_000);
        assert!(message_state.addr_state.over_limit());
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
}
