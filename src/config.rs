use std::{collections::HashSet, path::PathBuf};

use bitcoin::ScriptBuf;

use crate::{
    chain::checkpoints::HeaderCheckpoint,
    network::{dns::DnsConfig, ConnectionType},
    LogLevel, PeerStoreSizeConfig, PeerTimeoutConfig, TrustedPeer,
};

const REQUIRED_PEERS: u8 = 1;

pub(crate) struct NodeConfig {
    pub required_peers: u8,
    pub white_list: Vec<TrustedPeer>,
    pub dns_config: DnsConfig,
    pub addresses: HashSet<ScriptBuf>,
    pub data_path: Option<PathBuf>,
    pub header_checkpoint: Option<HeaderCheckpoint>,
    pub connection_type: ConnectionType,
    pub target_peer_size: PeerStoreSizeConfig,
    pub peer_timeout_config: PeerTimeoutConfig,
    pub log_level: LogLevel,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            required_peers: REQUIRED_PEERS,
            white_list: Default::default(),
            dns_config: DnsConfig::default(),
            addresses: Default::default(),
            data_path: Default::default(),
            header_checkpoint: Default::default(),
            connection_type: Default::default(),
            target_peer_size: PeerStoreSizeConfig::default(),
            peer_timeout_config: PeerTimeoutConfig::default(),
            log_level: Default::default(),
        }
    }
}
