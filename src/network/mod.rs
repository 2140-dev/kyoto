use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use bitcoin::{
    consensus::Decodable,
    io::Read,
    p2p::{address::AddrV2, message::CommandString, Magic},
};
use socks::create_socks5;
use tokio::{net::TcpStream, time::Instant};

use error::PeerError;

pub(crate) mod counter;
pub(crate) mod dns;
#[allow(dead_code)]
pub(crate) mod error;
pub(crate) mod outbound_messages;
pub(crate) mod parsers;
pub(crate) mod peer;
pub(crate) mod peer_map;
#[allow(dead_code)]
pub(crate) mod reader;
pub(crate) mod socks;

pub const PROTOCOL_VERSION: u32 = 70016;
pub const KYOTO_VERSION: &str = "0.10.0";
pub const RUST_BITCOIN_VERSION: &str = "0.32.4";

const THIRTY_MINS: u64 = 60 * 30;
const MESSAGE_TIMEOUT_SECS: u64 = 5;
//                    sec  min  hour
const TWO_HOUR: u64 = 60 * 60 * 2;
const TCP_CONNECTION_TIMEOUT: u64 = 2;

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
            response_timeout: Duration::from_secs(MESSAGE_TIMEOUT_SECS),
            max_connection_time: Duration::from_secs(TWO_HOUR),
            handshake_timeout: Duration::from_secs(TCP_CONNECTION_TIMEOUT),
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
            return Instant::now().duration_since(time) > Duration::from_secs(THIRTY_MINS);
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
    use std::net::Ipv4Addr;

    use bitcoin::p2p::address::AddrV2;

    use crate::prelude::Netgroup;

    #[test]
    fn test_sixteen() {
        let peer = AddrV2::Ipv4(Ipv4Addr::new(95, 217, 198, 121));
        assert_eq!("95.217".to_string(), peer.netgroup());
    }
}
