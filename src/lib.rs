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

use crate::network::{ConnectionType, PeerTimeoutConfig};

mod network;

mod broadcaster;
/// Convenient way to build a compact filters node.
pub mod builder;
/// Structures to communicate with a node.
pub mod client;
/// Errors associated with a node.
pub mod error;
/// Messages the node may send a client.
pub mod messages;
/// The structure that communicates with the Bitcoin P2P network and collects data.
pub mod node;

use bitcoin::OutPoint;
use chain::Filter;

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

// Re-exports
#[doc(inline)]
pub use chain::checkpoints::HashCheckpoint;

#[doc(inline)]
pub use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

#[doc(inline)]
pub use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

#[doc(inline)]
pub use {
    crate::builder::Builder,
    crate::chain::ChainState,
    crate::client::{Client, EventReceiver, Requester},
    crate::error::{ClientError, NodeError},
    crate::messages::{Event, Info, SyncUpdate, Warning},
    crate::node::Node,
};

#[doc(inline)]
pub use bitcoin::{
    bip158::BlockFilter, block::Header, p2p::address::AddrV2, p2p::ServiceFlags, Block, BlockHash,
    FeeRate, Network, ScriptBuf, Transaction, Wtxid,
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

/// The type of filter to make a request for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum FilterType {
    #[default]
    /// A golomb coded compact sketch based on siphash. Contains all spendable script types.
    Basic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BlockType {
    #[default]
    Legacy,
    Witness,
}

impl From<FilterType> for u8 {
    fn from(value: FilterType) -> Self {
        match value {
            FilterType::Basic => 0x00,
        }
    }
}

/// A compact block filter with associated height.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFilter {
    height: u32,
    header: Header,
    filter: Filter,
}

impl IndexedFilter {
    fn new(height: u32, header: Header, filter: Filter) -> Self {
        Self {
            height,
            header,
            filter,
        }
    }

    /// The height in the chain.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Return the [`BlockHash`] associated with this filer
    pub fn block_hash(&self) -> BlockHash {
        self.filter.block_hash()
    }

    /// Return the underlying block [`Header`] corresponding to this filter.
    pub fn header(&self) -> Header {
        self.header
    }

    /// Does the filter contain a positive match for any of the provided scripts
    pub fn contains_any<'a>(&'a self, scripts: impl Iterator<Item = &'a ScriptBuf>) -> bool {
        self.filter.contains_any(scripts)
    }

    /// Consume the index and get underlying block filter.
    pub fn block_filter(self) -> BlockFilter {
        self.filter.into_filter()
    }

    /// Consume the index and get the underlying parts.
    pub fn into_parts(self) -> (Header, BlockFilter) {
        (self.header, self.filter.into_filter())
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

#[derive(Debug, Clone)]
enum TrustedPeerInner {
    Addr(AddrV2),
    Hostname(String),
}

impl From<AddrV2> for TrustedPeerInner {
    fn from(addr: AddrV2) -> Self {
        Self::Addr(addr)
    }
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
///
/// // Or from a hostname — resolution happens at connection time.
/// let trusted = TrustedPeer::from_hostname("bitcoind.svc.local", 8333);
/// ```
#[derive(Debug, Clone)]
pub struct TrustedPeer {
    // The address or hostname of the remote node to connect to.
    address: TrustedPeerInner,
    // The port to establish a TCP connection. If none is provided, the typical Bitcoin Core port is used as the default.
    port: Option<u16>,
    // The services this peer is known to offer before starting the node.
    known_services: ServiceFlags,
}

impl TrustedPeer {
    /// Create a new trusted peer.
    pub fn new(address: impl Into<AddrV2>, port: Option<u16>, services: ServiceFlags) -> Self {
        Self {
            address: TrustedPeerInner::Addr(address.into()),
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
            address: TrustedPeerInner::Addr(address),
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
            address: TrustedPeerInner::Addr(address),
            port: Some(socket_addr.port()),
            known_services: ServiceFlags::NONE,
        }
    }

    /// Create a new trusted peer from a DNS hostname.
    ///
    /// The hostname is stored as-is and resolved to an IP address at the
    /// time a connection is attempted, via [`tokio::net::lookup_host`], or through
    /// the Socks5 proxy if one is configured. If resolution fails or yields no
    /// addresses, the peer is skipped and the next configured peer is tried.
    pub fn from_hostname(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            address: TrustedPeerInner::Hostname(hostname.into()),
            port: Some(port),
            known_services: ServiceFlags::NONE,
        }
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

/// Route network traffic through a Socks5 proxy, typically used by a Tor daemon.
#[derive(Debug, Clone)]
pub struct Socks5Proxy(SocketAddr);

impl Socks5Proxy {
    /// Define a non-standard Socks5 proxy to connect to.
    pub fn new(socket_addr: impl Into<SocketAddr>) -> Self {
        Self(socket_addr.into())
    }

    /// Connect to the default local Socks5 proxy hosted at `127.0.0.1:9050`.
    pub const fn local() -> Self {
        Socks5Proxy(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            9050,
        ))
    }
}

impl From<SocketAddr> for Socks5Proxy {
    fn from(value: SocketAddr) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug)]
struct Config {
    required_peers: u8,
    white_list: Vec<TrustedPeer>,
    whitelist_only: bool,
    chain_state: Option<ChainState>,
    connection_type: ConnectionType,
    peer_timeout_config: PeerTimeoutConfig,
    filter_type: FilterType,
    block_type: BlockType,
    headers_only_sync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            required_peers: 1,
            white_list: Default::default(),
            whitelist_only: Default::default(),
            chain_state: Default::default(),
            connection_type: Default::default(),
            peer_timeout_config: PeerTimeoutConfig::default(),
            filter_type: FilterType::default(),
            block_type: BlockType::default(),
            headers_only_sync: Default::default(),
        }
    }
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

fn default_port_from_network(network: &Network) -> u16 {
    match network {
        Network::Bitcoin => 8333,
        Network::Testnet => 18333,
        Network::Testnet4 => 48333,
        Network::Signet => 38333,
        Network::Regtest => 18444,
    }
}

#[derive(Debug, Clone)]
struct Dialog {
    info_tx: Sender<Info>,
    warn_tx: Sender<Warning>,
    event_tx: UnboundedSender<Event>,
    pending_events: Arc<AtomicUsize>,
}

impl Dialog {
    // An upper bound on the amount of pending events. Limited systems may not have sufficient
    // memory to hold pending filters in the channel.
    const MAX_PENDING_EVENTS: usize = 10_000;

    fn new(
        info_tx: Sender<Info>,
        warn_tx: Sender<Warning>,
        event_tx: UnboundedSender<Event>,
    ) -> Self {
        Self {
            info_tx,
            warn_tx,
            event_tx,
            pending_events: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn send_warning(&self, warning: Warning) {
        let _ = self.warn_tx.try_send(warning);
    }

    fn send_info(&self, info: Info) {
        let _ = self.info_tx.try_send(info);
    }

    fn send_event(&self, message: Event) {
        self.pending_events.fetch_add(1, Ordering::Relaxed);
        let _ = self.event_tx.send(message);
    }

    // If the events in a channel exceed a certain capacity, we temporarily stop fetching filters to
    // avoid OOM on limited systems. `Info` and `Warning` messages have a maximum channel size
    // before messages are dropped, so this does not apply. We allow going over this budget in the
    // case of header chain changes, which are important to process and do not consume high amounts
    // of memory.
    fn can_fit_filter_batch(&self) -> bool {
        self.pending_events.load(Ordering::Relaxed) + (chain::chain::FILTER_BATCH_SIZE as usize)
            < Self::MAX_PENDING_EVENTS
    }
}

/// A package is a set of dependent transactions to submit to the mempool.
#[derive(Debug, Clone)]
pub struct Package {
    parent: Transaction,
    child: Option<Transaction>,
}

impl Package {
    /// Create a new package from a single transaction.
    pub fn new_single(transaction: Transaction) -> Self {
        Self {
            parent: transaction,
            child: None,
        }
    }

    /// Construct a new package using the one-parent-one-child topology, where the child spends an
    /// output from the parent. The primary use of such a topology is for a child to bump the
    /// fee-rate of the package.
    ///
    /// # Errors
    ///
    /// If the child does not spend at least one output created by the parent.
    pub fn new_one_parent_one_child(
        parent: Transaction,
        child: Transaction,
    ) -> Result<Self, error::PackageError> {
        Self::child_depends_on_parent(&parent, &child)?;
        Ok(Self {
            parent,
            child: Some(child),
        })
    }

    /// Construct a new package from a list of transactions. Currently, the only valid package
    /// lengths are 1 and 2. In the case of two transactions, the child is expected to be _last_ in
    /// the list.
    ///
    /// # Errors
    ///
    /// - If the length of the vector is not one or two
    /// - The transactions do not depend on each other.
    pub fn from_vec(mut transactions: Vec<Transaction>) -> Result<Self, error::PackageError> {
        match transactions.len() {
            1 => Ok(Package::new_single(transactions.remove(0))),
            2 => {
                let parent = transactions.remove(0);
                let child = transactions.remove(0);
                Ok(Package::new_one_parent_one_child(parent, child)?)
            }
            invalid => Err(error::PackageError::InvalidPackageLength(invalid)),
        }
    }

    fn child_depends_on_parent(
        parent: &Transaction,
        child: &Transaction,
    ) -> Result<(), error::PackageError> {
        let outpoints = {
            let txid = parent.compute_txid();
            let mut outpoints = Vec::with_capacity(parent.output.len());
            for vout in 0..parent.output.len() {
                outpoints.push(OutPoint {
                    txid,
                    vout: vout as u32,
                });
            }
            outpoints
        };
        if !child
            .input
            .iter()
            .any(|input| outpoints.contains(&input.previous_output))
        {
            return Err(error::PackageError::UnrelatedTransactions);
        }
        Ok(())
    }

    fn advertise_package(&self) -> Wtxid {
        match &self.child {
            Some(child) => child.compute_wtxid(),
            None => self.parent.compute_wtxid(),
        }
    }

    fn parent(&self) -> Transaction {
        self.parent.clone()
    }

    fn child(&self) -> Option<Transaction> {
        self.child.clone()
    }
}

impl From<Transaction> for Package {
    fn from(value: Transaction) -> Self {
        Package {
            parent: value,
            child: None,
        }
    }
}

/// Instruct the node to watch for unconfirmed transactions with a certain criteria.
#[derive(Debug, Clone)]
pub struct GossipMonitorRequest {
    scripts: HashSet<ScriptBuf>,
    txins: HashSet<OutPoint>,
}

/// Build a new [`GossipMonitorRequest`].
#[derive(Debug, Clone, Default)]
pub struct GossipMonitorRequestBuilder {
    scripts: HashSet<ScriptBuf>,
    txins: HashSet<OutPoint>,
}

impl GossipMonitorRequestBuilder {
    /// Watch for these **output** scripts in an unconfirmed transactions.
    pub fn from_expected_output_scripts(scripts: impl IntoIterator<Item = ScriptBuf>) -> Self {
        let mut script_set = HashSet::new();
        script_set.extend(scripts);
        Self {
            scripts: script_set,
            txins: HashSet::new(),
        }
    }

    /// Watch for these [`OutPoint`] used as an input in unconfirmed transactions.
    pub fn from_expected_inputs(txins: impl IntoIterator<Item = OutPoint>) -> Self {
        let mut op_set = HashSet::new();
        op_set.extend(txins);
        Self {
            scripts: HashSet::new(),
            txins: op_set,
        }
    }

    /// Add scripts to the request.
    pub fn add_scripts(mut self, scripts: impl IntoIterator<Item = ScriptBuf>) -> Self {
        self.scripts.extend(scripts);
        self
    }

    /// Add outpoints to the request.
    pub fn add_outpoints(mut self, txins: impl IntoIterator<Item = OutPoint>) -> Self {
        self.txins.extend(txins);
        self
    }

    /// Complete the configuration for the request.
    pub fn build(self) -> GossipMonitorRequest {
        GossipMonitorRequest {
            scripts: self.scripts,
            txins: self.txins,
        }
    }
}

macro_rules! impl_sourceless_error {
    ($e:ident) => {
        impl std::error::Error for $e {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                None
            }
        }
    };
}

pub(crate) use impl_sourceless_error;

macro_rules! debug {
    ($expr:expr) => {
        #[cfg(debug_assertions)]
        if std::env::var("KYOTO_DEBUG_OUTPUT").is_ok() {
            println!("{}", $expr)
        }
    };
}

pub(crate) use debug;

#[cfg(test)]
macro_rules! impl_deserialize {
    ($t:ident, $for:ident) => {
        impl<'de> serde::Deserialize<'de> for $t {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
                let bitcoin_type: $for =
                    bitcoin::consensus::deserialize(&bytes).map_err(serde::de::Error::custom)?;
                Ok($t(bitcoin_type))
            }
        }
    };
}

#[cfg(test)]
pub(crate) use impl_deserialize;
