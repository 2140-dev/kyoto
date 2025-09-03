use std::net::{IpAddr, SocketAddr};
use std::{path::PathBuf, time::Duration};

use bitcoin::Network;
#[cfg(not(feature = "filter-control"))]
use bitcoin::ScriptBuf;

use super::{client::Client, config::NodeConfig, node::Node};
#[cfg(feature = "rusqlite")]
use crate::db::error::SqlInitializationError;
#[cfg(feature = "rusqlite")]
use crate::db::sqlite::peers::SqlitePeerDb;
use crate::network::dns::{DnsResolver, DNS_RESOLVER_PORT};
use crate::network::ConnectionType;
use crate::{chain::checkpoints::HeaderCheckpoint, db::traits::PeerStore};
use crate::{LogLevel, PeerStoreSizeConfig, TrustedPeer};

#[cfg(feature = "rusqlite")]
/// The default node returned from the [`NodeBuilder`].
pub type NodeDefault = Node<SqlitePeerDb>;

const MIN_PEERS: u8 = 1;
const MAX_PEERS: u8 = 15;

/// Build a [`Node`] in an additive way.
///
/// # Examples
///
/// Nodes may be built with minimal configuration.
///
/// ```rust
/// use std::net::{IpAddr, Ipv4Addr};
/// use std::collections::HashSet;
/// use kyoto::{NodeBuilder, Network};
///
/// let host = (IpAddr::from(Ipv4Addr::new(0, 0, 0, 0)), None);
/// let builder = NodeBuilder::new(Network::Regtest);
/// let (node, client) = builder
///     .add_peers(vec![host.into()])
///     .build()
///     .unwrap();
/// ```
///
/// Or, more practically, known Bitcoin scripts to monitor for may be added
/// as the node is built.
///
/// ```rust
/// use std::collections::HashSet;
/// use std::str::FromStr;
/// use kyoto::{HeaderCheckpoint, NodeBuilder, Network, Address, BlockHash};
///
/// let address = Address::from_str("tb1q9pvjqz5u5sdgpatg3wn0ce438u5cyv85lly0pc")
///     .unwrap()
///     .require_network(Network::Signet)
///     .unwrap()
///     .into();
/// let mut script_set = HashSet::new();
/// script_set.insert(address);
/// let checkpoint = HeaderCheckpoint::closest_checkpoint_below_height(170_000, Network::Signet);
///
/// let builder = NodeBuilder::new(Network::Signet);
/// // Add node preferences and build the node/client
/// let (mut node, client) = builder
///     // The Bitcoin scripts to monitor
///     .add_scripts(script_set)
///     // Only scan blocks strictly after a checkpoint
///     .after_checkpoint(checkpoint)
///     // The number of connections we would like to maintain
///     .required_peers(2)
///     .build()
///     .unwrap();
/// ```
pub struct NodeBuilder {
    config: NodeConfig,
    network: Network,
}

impl NodeBuilder {
    /// Create a new [`NodeBuilder`].
    pub fn new(network: Network) -> Self {
        Self {
            config: NodeConfig::default(),
            network,
        }
    }

    /// Fetch the [`Network`] for the builder.
    pub fn network(&self) -> Network {
        self.network
    }

    /// Add preferred peers to try to connect to.
    pub fn add_peers(mut self, whitelist: impl IntoIterator<Item = TrustedPeer>) -> Self {
        self.config.white_list.extend(whitelist);
        self
    }

    /// Add a preferred peer to try to connect to.
    pub fn add_peer(mut self, trusted_peer: impl Into<TrustedPeer>) -> Self {
        self.config.white_list.push(trusted_peer.into());
        self
    }

    /// Add Bitcoin scripts to monitor for. You may add more later with the [`Client`].
    #[cfg(not(feature = "filter-control"))]
    pub fn add_scripts(mut self, scripts: impl IntoIterator<Item = ScriptBuf>) -> Self {
        let script_iter = scripts.into_iter();
        for script in script_iter {
            self.config.addresses.insert(script);
        }
        self
    }

    /// Add a path to the directory where data should be stored. If none is provided, the current
    /// working directory will be used.
    pub fn data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.data_path = Some(path.into());
        self
    }

    /// Add the minimum number of peer connections that should be maintained by the node.
    /// Adding more connections increases the node's anonymity, but requires waiting for more responses,
    /// higher bandwidth, and higher memory requirements. If none is provided, a single connection will be maintained.
    /// The number of connections will be clamped to a range of 1 to 15.
    pub fn required_peers(mut self, num_peers: u8) -> Self {
        self.config.required_peers = num_peers.clamp(MIN_PEERS, MAX_PEERS);
        self
    }

    /// Set the desired number of peers for the database to keep track of. For limited or in-memory peer storage,
    /// this number may be small, however a sufficient margin of peers should be set so the node can try many options
    /// when downloading compact block filters. For nodes that store peers on disk, more peers will typically result in
    /// fewer errors. If none is provided, no limit to the size of the store will be introduced.
    pub fn peer_db_size(mut self, target: PeerStoreSizeConfig) -> Self {
        self.config.target_peer_size = target;
        self
    }

    /// Add a checkpoint for the node to look for relevant blocks _strictly after_ the given height.
    /// This may be from the same [`HeaderCheckpoint`] every time the node is ran, or from the last known sync height.
    /// In the case of a block reorganization, the node may scan for blocks below the given block height
    /// to accurately reflect which relevant blocks are in the best chain.
    /// If none is provided, the _most recent_ checkpoint will be used.
    pub fn after_checkpoint(mut self, checkpoint: impl Into<HeaderCheckpoint>) -> Self {
        self.config.header_checkpoint = Some(checkpoint.into());
        self
    }

    /// Set the [`LogLevel`]. Omitting log messages may improve performance.
    pub fn log_level(mut self, log_level: LogLevel) -> Self {
        self.config.log_level = log_level;
        self
    }

    /// Set the time a peer has to complete the initial TCP handshake. Even on unstable
    /// connections this may be fast.
    ///
    /// If none is provided, a timeout of two seconds will be used.
    pub fn handshake_timeout(mut self, handshake_timeout: impl Into<Duration>) -> Self {
        self.config.peer_timeout_config.handshake_timeout = handshake_timeout.into();
        self
    }

    /// Set the time duration a peer has to respond to a message from the local node.
    ///
    /// ## Note
    ///
    /// Both bandwidth and computing time should be considered when configuring this timeout.
    /// On test networks, this value may be quite short, however on the Bitcoin network,
    /// nodes may be slower to respond while processing blocks and transactions.
    ///
    /// If none is provided, a timeout of 5 seconds will be used.
    pub fn response_timeout(mut self, response_timeout: impl Into<Duration>) -> Self {
        self.config.peer_timeout_config.response_timeout = response_timeout.into();
        self
    }

    /// The maximum connection time that will be maintained with a remote peer, regardless of
    /// the quality of the peer.
    ///
    /// ## Note
    ///
    /// This value is configurable as some developers may be satisfied with a peer
    /// as long as the peer responds promptly. Other implementations may value finding
    /// new and reliable peers faster, so the maximum connection time may be shorter.
    ///
    /// If none is provided, a maximum connection time of two hours will be used.
    pub fn maximum_connection_time(mut self, max_connection_time: impl Into<Duration>) -> Self {
        self.config.peer_timeout_config.max_connection_time = max_connection_time.into();
        self
    }

    /// Configure the DNS resolver to use when querying DNS seeds.
    /// Default is `1.1.1.1:53`.
    pub fn dns_resolver(mut self, resolver: impl Into<IpAddr>) -> Self {
        let ip_addr = resolver.into();
        let socket_addr = SocketAddr::new(ip_addr, DNS_RESOLVER_PORT);
        self.config.dns_resolver = DnsResolver { socket_addr };
        self
    }

    /// Route network traffic through a Tor daemon using a Socks5 proxy. Currently, proxies
    /// must be reachable by IP address.
    pub fn socks5_proxy(mut self, proxy: impl Into<SocketAddr>) -> Self {
        let ip_addr = proxy.into();
        let connection = ConnectionType::Socks5Proxy(ip_addr);
        self.config.connection_type = connection;
        self
    }

    /// Consume the node builder and receive a [`Node`] and [`Client`].
    ///
    /// # Errors
    ///
    /// Building a node and client will error if a database connection is denied or cannot be found.
    #[cfg(feature = "rusqlite")]
    pub fn build(&mut self) -> Result<(NodeDefault, Client), SqlInitializationError> {
        let peer_store = SqlitePeerDb::new(self.network, self.config.data_path.clone())?;
        Ok(Node::new(
            self.network,
            core::mem::take(&mut self.config),
            peer_store,
        ))
    }

    /// Consume the node builder by using custom database implementations, receiving a [`Node`] and [`Client`].
    pub fn build_with_databases<P: PeerStore + 'static>(
        &mut self,
        peer_store: P,
    ) -> (Node<P>, Client) {
        Node::new(self.network, core::mem::take(&mut self.config), peer_store)
    }
}
