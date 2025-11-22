use bitcoin::{
    block::Header,
    hashes::Hash,
    p2p::{
        address::AddrV2Message,
        message::NetworkMessage,
        message_blockdata::Inventory,
        message_filter::{CFHeaders, CFilter},
        message_network::VersionMessage,
        ServiceFlags,
    },
    Block, BlockHash,
};
use bitcoin::{FeeRate, Wtxid};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc::Sender;

use crate::messages::RejectPayload;

use super::error::ReaderError;
use super::inbound::MessageParser;
use super::TimeSensitiveId;

// From Bitcoin Core PR #29575
const MAX_ADDR: usize = 1_000;
const MAX_INV: usize = 50_000;
const MAX_HEADERS: usize = 2_000;

pub(in crate::network) struct Reader<R: AsyncBufReadExt + Send + Sync + Unpin> {
    parser: MessageParser<R>,
    tx: Sender<ReaderMessage>,
}

impl<R: AsyncBufReadExt + Send + Sync + Unpin> Reader<R> {
    pub fn new(parser: MessageParser<R>, tx: Sender<ReaderMessage>) -> Self {
        Self { parser, tx }
    }

    pub(in crate::network) async fn read_from_remote(&mut self) -> Result<(), ReaderError> {
        loop {
            if let Some(message) = self.parser.read_message().await? {
                let cleaned_message = self.parse_message(message);
                match cleaned_message {
                    Some(message) => self.tx.send(message).await?,
                    None => continue,
                }
            }
        }
    }

    fn parse_message(&self, message: NetworkMessage) -> Option<ReaderMessage> {
        // Supported messages are protocol version 70013 and below
        match message {
            NetworkMessage::Version(version) => Some(ReaderMessage::Version(version)),
            NetworkMessage::Verack => Some(ReaderMessage::Verack),
            // If a peer is sending this message they are incredibly old or faulty.
            NetworkMessage::Addr(_) => None,
            NetworkMessage::Inv(inventory) => {
                if inventory.len() > MAX_INV {
                    return Some(ReaderMessage::Disconnect);
                }
                let mut hashes = Vec::new();
                for i in inventory {
                    match i {
                        Inventory::Block(hash) => hashes.push(hash),
                        Inventory::CompactBlock(hash) => hashes.push(hash),
                        Inventory::WitnessBlock(hash) => hashes.push(hash),
                        _ => continue,
                    }
                }
                if !hashes.is_empty() {
                    Some(ReaderMessage::NewBlocks(hashes))
                } else {
                    None
                }
            }
            NetworkMessage::GetData(inventory) => {
                let mut requests = Vec::new();
                for inv in inventory {
                    match inv {
                        Inventory::WTx(wtxid) => requests.push(wtxid),
                        _ => continue,
                    }
                }
                Some(ReaderMessage::TxRequests(requests))
            }
            NetworkMessage::NotFound(_) => None,
            NetworkMessage::GetBlocks(_) => None,
            NetworkMessage::GetHeaders(_) => None,
            NetworkMessage::MemPool => None,
            NetworkMessage::Tx(_) => None,
            NetworkMessage::Block(block) => Some(ReaderMessage::Block(block)),
            NetworkMessage::Headers(headers) => {
                if headers.len() > MAX_HEADERS {
                    return Some(ReaderMessage::Disconnect);
                }
                Some(ReaderMessage::Headers(headers))
            }
            // 70012
            NetworkMessage::SendHeaders => None,
            NetworkMessage::GetAddr => None,
            NetworkMessage::Ping(nonce) => Some(ReaderMessage::Ping(nonce)),
            NetworkMessage::Pong(nonce) => Some(ReaderMessage::Pong(nonce)),
            NetworkMessage::MerkleBlock(_) => None,
            // Bloom Filters are enabled by 70011
            NetworkMessage::FilterLoad(_) => None,
            NetworkMessage::FilterAdd(_) => None,
            NetworkMessage::FilterClear => None,
            NetworkMessage::GetCFilters(_) => None,
            NetworkMessage::CFilter(filter) => Some(ReaderMessage::Filter(filter)),
            NetworkMessage::GetCFHeaders(_) => None,
            NetworkMessage::CFHeaders(cf_headers) => Some(ReaderMessage::FilterHeaders(cf_headers)),
            NetworkMessage::GetCFCheckpt(_) => None,
            NetworkMessage::CFCheckpt(_) => None,
            // Compact Block Relay is enabled with 70014
            NetworkMessage::SendCmpct(_) => None,
            NetworkMessage::CmpctBlock(_) => None,
            NetworkMessage::GetBlockTxn(_) => None,
            NetworkMessage::BlockTxn(_) => None,
            NetworkMessage::Alert(_) => None,
            NetworkMessage::Reject(rejection) => {
                let wtxid = Wtxid::from(rejection.hash);
                Some(ReaderMessage::Reject(RejectPayload {
                    reason: Some(rejection.ccode),
                    wtxid,
                }))
            }
            // 70013
            NetworkMessage::FeeFilter(i) => {
                if i < 0 {
                    Some(ReaderMessage::Disconnect)
                } else {
                    // Safe cast because i64::MAX < u64::MAX
                    let fee_rate = FeeRate::from_sat_per_kwu(i as u64 / 4);
                    Some(ReaderMessage::FeeFilter(fee_rate))
                }
            }
            // 70016
            NetworkMessage::WtxidRelay => None,
            NetworkMessage::AddrV2(addresses) => {
                if addresses.len() > MAX_ADDR {
                    return Some(ReaderMessage::Disconnect);
                }
                let addresses = addresses
                    .into_iter()
                    .filter(|f| {
                        f.services.has(ServiceFlags::COMPACT_FILTERS)
                            && f.services.has(ServiceFlags::NETWORK)
                    })
                    .collect::<Vec<AddrV2Message>>();
                if addresses.is_empty() {
                    return None;
                }
                Some(ReaderMessage::Addr(addresses))
            }
            NetworkMessage::SendAddrV2 => None,
            #[allow(unused)]
            NetworkMessage::Unknown { command, payload } => Some(ReaderMessage::Disconnect),
        }
    }
}

#[derive(Debug)]
pub(in crate::network) enum ReaderMessage {
    Version(VersionMessage),
    Addr(Vec<AddrV2Message>),
    Headers(Vec<Header>),
    FilterHeaders(CFHeaders),
    Filter(CFilter),
    Block(Block),
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
    pub(in crate::network) fn time_sensitive_message_received(&self) -> Option<TimeSensitiveId> {
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
