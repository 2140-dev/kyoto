use std::collections::HashSet;

use bitcoin::{
    block::Header,
    hashes::Hash,
    p2p::{
        address::AddrV2,
        message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
        message_network::VersionMessage,
        ServiceFlags,
    },
    Block, BlockHash, FeeRate, Transaction, Txid, Wtxid,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{messages::RejectPayload, network::PeerId};

#[derive(Debug, Clone)]
pub(crate) enum MainThreadMessage {
    GetAddr,
    GetAddrV2,
    WtxidRelay,
    GetHeaders(GetHeaderConfig),
    GetFilterHeaders(GetCFHeaders),
    GetFilters(GetCFilters),
    GetBlock(GetBlockConfig),
    FetchTransactions(FetchTransactions),
    Disconnect,
    BroadcastPending,
    Verack,
}

impl MainThreadMessage {
    pub(crate) fn time_sensitive_message_start(&self) -> Option<(TimeSensitiveId, Instant)> {
        match self {
            MainThreadMessage::GetHeaders(_) => Some((TimeSensitiveId::HEADER_MSG, Instant::now())),
            MainThreadMessage::GetFilterHeaders(_) => {
                Some((TimeSensitiveId::CF_HEADER_MSG, Instant::now()))
            }
            MainThreadMessage::GetFilters(_) => {
                Some((TimeSensitiveId::C_FILTER_MSG, Instant::now()))
            }
            MainThreadMessage::GetBlock(conf) => {
                let id = conf.locator.to_raw_hash().to_byte_array();
                Some((TimeSensitiveId::from_slice(id), Instant::now()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GetHeaderConfig {
    pub locators: Vec<BlockHash>,
    pub stop_hash: Option<BlockHash>,
}

#[derive(Debug, Clone)]
pub struct GetBlockConfig {
    pub locator: BlockHash,
}

#[derive(Debug, Clone)]
pub struct FetchTransactions {
    pub sender: mpsc::Sender<(Txid, Transaction)>,
    pub txids: HashSet<Txid>,
}

impl FetchTransactions {
    pub fn new(sender: mpsc::Sender<(Txid, Transaction)>, txids: HashSet<Txid>) -> Self {
        Self { sender, txids }
    }
}

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
pub(crate) enum ReaderMessage {
    Version(VersionMessage),
    Addr(Vec<CombinedAddr>),
    Headers(Vec<Header>),
    FilterHeaders(CFHeaders),
    Filter(CFilter),
    Block(Block),
    Tx(Transaction),
    NewBlocks(Vec<BlockHash>),
    Reject(RejectPayload),
    Disconnect,
    Verack,
    Ping(u64),
    #[allow(dead_code)]
    Pong(u64),
    FeeFilter(FeeRate),
    TxRequests(Vec<Wtxid>),
}

impl ReaderMessage {
    pub(crate) fn time_sensitive_message_received(&self) -> Option<TimeSensitiveId> {
        match self {
            ReaderMessage::Headers(_) => Some(TimeSensitiveId::HEADER_MSG),
            ReaderMessage::FilterHeaders(_) => Some(TimeSensitiveId::CF_HEADER_MSG),
            ReaderMessage::Filter(_) => Some(TimeSensitiveId::C_FILTER_MSG),
            ReaderMessage::Pong(_) => Some(TimeSensitiveId::PING),
            ReaderMessage::Block(b) => {
                let hash = *b.block_hash().to_raw_hash().as_byte_array();
                Some(TimeSensitiveId::from_slice(hash))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CombinedAddr {
    pub addr: AddrV2,
    pub port: u16,
    pub services: ServiceFlags,
}

impl CombinedAddr {
    pub(crate) fn new(addr: AddrV2, port: u16) -> Self {
        Self {
            addr,
            port,
            services: ServiceFlags::NONE,
        }
    }

    pub(crate) fn services(&mut self, services: ServiceFlags) {
        self.services = services
    }
}

#[derive(Debug, Clone, Copy, std::hash::Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TimeSensitiveId([u8; 32]);

impl TimeSensitiveId {
    pub(crate) const HEADER_MSG: Self = Self([1; 32]);

    pub(crate) const CF_HEADER_MSG: Self = Self([2; 32]);

    pub(crate) const C_FILTER_MSG: Self = Self([3; 32]);

    pub(crate) const PING: Self = Self([4; 32]);

    pub(crate) fn from_slice(slice: [u8; 32]) -> Self {
        Self(slice)
    }
}
