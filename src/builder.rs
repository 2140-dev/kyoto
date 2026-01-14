use std::net::SocketAddr;
use std::{path::PathBuf, time::Duration};

use bitcoin::Network;

use super::{client::Client, node::Node};
use crate::chain::ChainState;
use crate::network::ConnectionType;
use crate::TrustedPeer;
use crate::{Config, FilterType};

const MIN_PEERS: u8 = 1;
const MAX_PEERS: u8 = 15;

/// Build a [`Node`] in an additive way.
///
/// # Examples
///
/// Nodes may be built with minimal configuration.
///
/// ```no_run
/// use std::net::{IpAddr, Ipv4Addr};
/// use std::collections::HashSet;
/// use bip157::{Builder, Network};
///
/// let host = (IpAddr::from(Ipv4Addr::new(0, 0, 0, 0)), None);
/// let builder = Builder::new(Network::Regtest);
/// let (node, client) = builder
///     .add_peers(vec![host.into()])
///     .build();
/// ```
#[derive(Debug)]
pub struct Builder {
    config: Config,
    network: Network,
}

impl Builder {
    /// Create a new [`Builder`].
    pub fn new(network: Network) -> Self {
        Self {
            config: Config::default(),
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

    /// Initialize the chain state of the node with previous information or a starting checkpoint.
    /// This information will be used to inform the client of any block reorganizations and to
    /// enforce consensus rules on proof of work.
    pub fn chain_state(mut self, state: ChainState) -> Self {
        self.config.chain_state = Some(state);
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

    /// Route network traffic through a Tor daemon using a Socks5 proxy. Currently, proxies
    /// must be reachable by IP address.
    pub fn socks5_proxy(mut self, proxy: impl Into<SocketAddr>) -> Self {
        let ip_addr = proxy.into();
        let connection = ConnectionType::Socks5Proxy(ip_addr);
        self.config.connection_type = connection;
        self
    }

    /// Request a specific compact sketch type.
    pub fn filter_type(mut self, filter_type: FilterType) -> Self {
        self.config.filter_type = filter_type;
        self
    }

    /// Consume the node builder and receive a [`Node`] and [`Client`].
    pub fn build(mut self) -> (Node, Client) {
        Node::new(self.network, core::mem::take(&mut self.config))
    }
}
