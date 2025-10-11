//! This crate is a conservative, private, and vetted Bitcoin client built in accordance
//! with the [BIP157](https://github.com/bitcoin/bips/blob/master/bip-0157.mediawiki) and [BIP158](https://github.com/bitcoin/bips/blob/master/bip-0158.mediawiki)
//! standards. _Conservative_, as in this crate makes very little assumptions about the underlying memory requirements of the
//! device running the software. _Private_, as in the Bitcoin nodes that serve this client data do not know what transactions the
//! client is querying for, only the entire Bitcoin block. _Vetted_, as in the dependencies of the core library are meant to remain limited,
//! rigorously tested, and absolutely necessary.
//!
//! # Example usage
//!
//! ```no_run
//! use bip157::{Builder, Event, Client, Network, BlockHash};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Add third-party logging
//!     let subscriber = tracing_subscriber::FmtSubscriber::new();
//!     tracing::subscriber::set_global_default(subscriber).unwrap();
//!     // Create a new node builder
//!     let builder = Builder::new(Network::Signet);
//!     // Add node preferences and build the node/client
//!     let (node, client) = builder
//!         // The number of connections we would like to maintain
//!         .required_peers(2)
//!         .build();
//!     // Run the node and wait for the sync message;
//!     tokio::task::spawn(async move { node.run().await });
//!     // Split the client into components that send messages and listen to messages
//!     let Client { requester, info_rx: _, warn_rx: _, mut event_rx } = client;
//!     loop {
//!         if let Some(event) = event_rx.recv().await {
//!             match event {
//!                 Event::FiltersSynced(_) => {
//!                     tracing::info!("Sync complete!");
//!                     break;
//!                 },
//!                 _ => (),
//!             }
//!         }
//!     }
//!     requester.shutdown();
//! }
//! ```

#![warn(missing_docs)]
pub mod chain;

mod network;
mod prelude;
pub(crate) use prelude::impl_sourceless_error;

mod broadcaster;
/// Convenient way to build a compact filters node.
pub mod builder;
pub(crate) mod channel_messages;
/// Structures to communicate with a node.
pub mod client;
/// Node configuration options.
pub(crate) mod config;
pub(crate) mod dialog;
/// Errors associated with a node.
pub mod error;
/// Messages the node may send a client.
pub mod messages;
/// The structure that communicates with the Bitcoin P2P network and collects data.
pub mod node;

use chain::Filter;

use std::net::{IpAddr, SocketAddr};

// Re-exports
#[doc(inline)]
pub use chain::checkpoints::HeaderCheckpoint;

#[doc(inline)]
pub use tokio::sync::mpsc::Receiver;
#[doc(inline)]
pub use tokio::sync::mpsc::UnboundedReceiver;

#[doc(inline)]
pub use {
    crate::builder::Builder,
    crate::client::{Client, Requester},
    crate::error::{ClientError, NodeError},
    crate::messages::{Event, Info, Progress, RejectPayload, SyncUpdate, Warning},
    crate::node::Node,
};

#[doc(inline)]
pub use bitcoin::{
    bip158::BlockFilter, block::Header, p2p::address::AddrV2, p2p::message_network::RejectReason,
    p2p::ServiceFlags, Address, Block, BlockHash, FeeRate, Network, ScriptBuf, Transaction, Wtxid,
};

pub extern crate tokio;

/// A Bitcoin [`Block`] with associated height.
#[derive(Debug, Clone)]
pub struct IndexedBlock {
    /// The height or index in the chain.
    pub height: u32,
    /// The Bitcoin block with some matching script.
    pub block: Block,
}

impl IndexedBlock {
    pub(crate) fn new(height: u32, block: Block) -> Self {
        Self { height, block }
    }
}

/// A compact block filter with associated height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFilter {
    height: u32,
    filter: Filter,
}

impl IndexedFilter {
    fn new(height: u32, filter: Filter) -> Self {
        Self { height, filter }
    }

    /// The height in the chain.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Return the [`BlockHash`] associated with this filer
    pub fn block_hash(&self) -> BlockHash {
        self.filter.block_hash()
    }

    /// Does the filter contain a positive match for any of the provided scripts
    pub fn contains_any<'a>(&'a self, scripts: impl Iterator<Item = &'a ScriptBuf>) -> bool {
        self.filter.contains_any(scripts)
    }

    /// Consume the index and get underlying block filter.
    pub fn block_filter(self) -> BlockFilter {
        self.filter.into_filter()
    }

    /// Consume the filter and get the raw bytes
    pub fn into_contents(self) -> Vec<u8> {
        self.filter.contents()
    }
}

impl std::cmp::PartialOrd for IndexedFilter {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for IndexedFilter {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.height.cmp(&other.height)
    }
}

/// Broadcast a [`Transaction`] to a set of connected peers.
#[derive(Debug, Clone)]
pub struct TxBroadcast {
    /// The presumably valid Bitcoin transaction.
    pub tx: Transaction,
    /// The strategy for how this transaction should be shared with the network.
    pub broadcast_policy: TxBroadcastPolicy,
}

impl TxBroadcast {
    /// Prepare a transaction for broadcast with associated broadcast strategy.
    pub fn new(tx: Transaction, broadcast_policy: TxBroadcastPolicy) -> Self {
        Self {
            tx,
            broadcast_policy,
        }
    }

    /// Prepare a transaction to be broadcasted to a random connection.
    pub fn random_broadcast(tx: Transaction) -> Self {
        Self {
            tx,
            broadcast_policy: TxBroadcastPolicy::RandomPeer,
        }
    }
}

/// The strategy for how this transaction should be shared with the network.
#[derive(Debug, Default, Clone)]
pub enum TxBroadcastPolicy {
    /// Broadcast the transaction to all peers at the same time.
    AllPeers,
    /// Broadcast the transaction to a single random peer, optimal for user privacy.
    #[default]
    RandomPeer,
}

/// A peer on the Bitcoin P2P network
///
/// # Building peers
///
/// ```rust
/// use std::net::{IpAddr, Ipv4Addr};
/// use bip157::{TrustedPeer, ServiceFlags, AddrV2};
///
/// let local_host = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
/// let mut trusted = TrustedPeer::from_ip(local_host);
/// // Optionally set the known services of the peer later.
/// trusted.set_services(ServiceFlags::P2P_V2);
///
/// let local_host = Ipv4Addr::new(0, 0, 0, 0);
/// // Or construct a trusted peer directly.
/// let trusted = TrustedPeer::new(AddrV2::Ipv4(local_host), None, ServiceFlags::P2P_V2);
///
/// // Or implicitly with `into`
/// let local_host = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
/// let trusted: TrustedPeer = (local_host, None).into();
/// ```
#[derive(Debug, Clone)]
pub struct TrustedPeer {
    /// The IP address of the remote node to connect to.
    pub address: AddrV2,
    /// The port to establish a TCP connection. If none is provided, the typical Bitcoin Core port is used as the default.
    pub port: Option<u16>,
    /// The services this peer is known to offer before starting the node.
    pub known_services: ServiceFlags,
}

impl TrustedPeer {
    /// Create a new trusted peer.
    pub fn new(address: AddrV2, port: Option<u16>, services: ServiceFlags) -> Self {
        Self {
            address,
            port,
            known_services: services,
        }
    }

    /// Create a new trusted peer using the default port for the network.
    pub fn from_ip(ip_addr: impl Into<IpAddr>) -> Self {
        let address = match ip_addr.into() {
            IpAddr::V4(ip) => AddrV2::Ipv4(ip),
            IpAddr::V6(ip) => AddrV2::Ipv6(ip),
        };
        Self {
            address,
            port: None,
            known_services: ServiceFlags::NONE,
        }
    }

    /// Create a new peer from a known address and port.
    pub fn from_socket_addr(socket_addr: impl Into<SocketAddr>) -> Self {
        let socket_addr: SocketAddr = socket_addr.into();
        let address = match socket_addr {
            SocketAddr::V4(ip) => AddrV2::Ipv4(*ip.ip()),
            SocketAddr::V6(ip) => AddrV2::Ipv6(*ip.ip()),
        };
        Self {
            address,
            port: Some(socket_addr.port()),
            known_services: ServiceFlags::NONE,
        }
    }

    /// The IP address of the trusted peer.
    pub fn address(&self) -> AddrV2 {
        self.address.clone()
    }

    /// A recommended port to connect to, if there is one.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// The services this peer is known to offer.
    pub fn services(&self) -> ServiceFlags {
        self.known_services
    }

    /// Set the known services for this trusted peer.
    pub fn set_services(&mut self, services: ServiceFlags) {
        self.known_services = services;
    }
}

impl From<(IpAddr, Option<u16>)> for TrustedPeer {
    fn from(value: (IpAddr, Option<u16>)) -> Self {
        let address = match value.0 {
            IpAddr::V4(ip) => AddrV2::Ipv4(ip),
            IpAddr::V6(ip) => AddrV2::Ipv6(ip),
        };
        TrustedPeer::new(address, value.1, ServiceFlags::NONE)
    }
}

impl From<TrustedPeer> for (AddrV2, Option<u16>) {
    fn from(value: TrustedPeer) -> Self {
        (value.address(), value.port())
    }
}

impl From<IpAddr> for TrustedPeer {
    fn from(value: IpAddr) -> Self {
        TrustedPeer::from_ip(value)
    }
}

impl From<SocketAddr> for TrustedPeer {
    fn from(value: SocketAddr) -> Self {
        TrustedPeer::from_socket_addr(value)
    }
}

#[derive(Debug, Clone, Copy)]
enum NodeState {
    // We are behind on block headers according to our peers.
    Behind,
    // We may start downloading compact block filter headers.
    HeadersSynced,
    // We may start scanning compact block filters.
    FilterHeadersSynced,
    // We may start asking for blocks with matches.
    FiltersSynced,
}

/// Query a Bitcoin DNS seeder.
///
/// This is **not** a generic DNS implementation. It is specifically tailored to query and parse DNS for Bitcoin seeders.
/// Iternally, three queries will be made with varying filters for the requested service flags.
/// Similar to [`lookup_host`](tokio::net::lookup_host), this has no guarantee to return any `IpAddr`.
///
/// # Example usage
///
/// ```no_run
/// use std::net::{IpAddr, Ipv4Addr};
///
/// use bip157::lookup_host;
///
/// #[tokio::main]
/// async fn main() {
///     let addrs = lookup_host("seed.bitcoin.sipa.be").await;
/// }
/// ```
pub async fn lookup_host<S: AsRef<str>>(hostname: S) -> Vec<IpAddr> {
    crate::network::dns::lookup_hostname(hostname.as_ref()).await
}

macro_rules! debug {
    ($expr:expr) => {
        #[cfg(debug_assertions)]
        println!("{}", $expr)
    };
}

pub(crate) use debug;
