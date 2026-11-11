use std::collections::BTreeMap;

use bitcoin::p2p::address::AddrV2;
use bitcoin::p2p::ServiceFlags;
use bitcoin::Transaction;
use bitcoin::{block::Header, BlockHash, FeeRate, Wtxid};
use tokio::sync::mpsc::Receiver;

use crate::chain::{BlockHeaderChanges, IndexedHeader};
use crate::{chain::checkpoints::HashCheckpoint, IndexedBlock, TrustedPeer};
use crate::{BlockType, GossipMonitorRequest, IndexedFilter, Package};

use super::error::FetchBlockError;

/// Informational messages emitted by a node
#[derive(Debug, Clone)]
pub enum Info {
    /// The node was able to successfully complete a version handshake.
    SuccessfulHandshake,
    /// The node is connected to all required peers.
    ConnectionsMet,
    /// The header chain has been extended to a new height.
    NewChainHeight(u32),
    /// Compact filter headers have been checked up to this height.
    CFHeadersAdded(u32),
    /// A compact block filter has been checked at this height.
    FilterAdded(u32),
    /// A requested block has been received and is being processed.
    BlockReceived(BlockHash),
}

impl core::fmt::Display for Info {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Info::SuccessfulHandshake => write!(f, "Successful version handshake with a peer"),
            Info::ConnectionsMet => write!(f, "Required connections met"),
            Info::NewChainHeight(height) => write!(f, "New chain height {height}"),
            Info::CFHeadersAdded(height) => write!(f, "Filter headers added to height {height}"),
            Info::FilterAdded(height) => write!(f, "Filter added at height {height}"),
            Info::BlockReceived(hash) => write!(f, "Received block {hash}"),
        }
    }
}

/// Data and structures useful for a consumer, such as a wallet.
#[derive(Debug, Clone)]
pub enum Event {
    /// The chain of block headers has been altered in some way.
    ChainUpdate(BlockHeaderChanges),
    /// The node is fully synced, having scanned the requested range.
    FiltersSynced(SyncUpdate),
    /// A compact block filter with associated height and block hash.
    IndexedFilter(IndexedFilter),
}

/// The node has synced to a new tip of the chain.
#[derive(Debug, Clone)]
pub struct SyncUpdate {
    /// Last known tip of the blockchain
    pub tip: HashCheckpoint,
    /// Ten recent headers ending with the tip
    pub recent_history: BTreeMap<u32, Header>,
}

impl SyncUpdate {
    pub(crate) fn new(tip: HashCheckpoint, recent_history: BTreeMap<u32, Header>) -> Self {
        Self {
            tip,
            recent_history,
        }
    }

    /// Get the tip of the blockchain after this sync.
    pub fn tip(&self) -> HashCheckpoint {
        self.tip
    }

    /// Get the ten most recent blocks in chronological order after this sync.
    pub fn recent_history(&self) -> &BTreeMap<u32, Header> {
        &self.recent_history
    }
}

/// Commands to issue a node.
#[derive(Debug)]
pub(crate) enum ClientMessage {
    /// Stop the node.
    Shutdown,
    /// Broadcast a [`crate::Transaction`] with a [`crate::TxBroadcastPolicy`].
    Broadcast(ClientRequest<Package, Wtxid>),
    /// Starting at the configured anchor checkpoint, re-emit all filters.
    Rescan(Option<u32>),
    /// Explicitly request a block from the node.
    GetBlock(ClientRequest<(BlockHash, Option<BlockType>), Result<IndexedBlock, FetchBlockError>>),
    /// Get the chain tip.
    BestBlock(ClientRequest<(), HashCheckpoint>),
    /// Add another known peer to connect to.
    AddPeer(TrustedPeer),
    /// Request the broadcast minimum fee rate.
    GetBroadcastMinFeeRate(ClientRequest<(), FeeRate>),
    /// Get info on connections
    GetPeerInfo(ClientRequest<(), Vec<(AddrV2, ServiceFlags)>>),
    /// Look up a header at a specific height in the chain of most work.
    GetHeader(ClientRequest<u32, Option<IndexedHeader>>),
    /// Look up the height of a block hash in the chain of most work.
    HeightOfHash(ClientRequest<BlockHash, Option<u32>>),
    /// Look for unconfirmed transactions.
    SubscribeGossip(ClientRequest<GossipMonitorRequest, Option<Receiver<Transaction>>>),
    /// Stop looking for unconfirmed transactions.
    UnsubscribeGossip,
    /// Send an empty message to see if the node is running.
    NoOp,
}

#[derive(Debug)]
pub(crate) struct ClientRequest<D: Clone, U> {
    data: D,
    response: tokio::sync::oneshot::Sender<U>,
}

impl<D: Clone, U> ClientRequest<D, U> {
    pub(crate) fn new(data: D, response: tokio::sync::oneshot::Sender<U>) -> Self {
        Self { data, response }
    }

    pub(crate) fn data(&self) -> D {
        self.data.clone()
    }

    pub(crate) fn into_values(self) -> (D, tokio::sync::oneshot::Sender<U>) {
        (self.data, self.response)
    }
}

/// Warnings a node may issue while running.
#[derive(Debug, Clone)]
pub enum Warning {
    /// The node is looking for connections to peers.
    NeedConnections {
        /// The number of live connections.
        connected: usize,
        /// The configured requirement.
        required: usize,
    },
    /// A connection to a peer timed out.
    PeerTimedOut,
    /// The node was unable to connect to a peer in the database.
    CouldNotConnect,
    /// A connection was maintained, but the peer does not signal for compact block filers.
    NoCompactFilters,
    /// The node has been waiting for new `inv` and will find new peers to avoid block withholding.
    PotentialStaleTip,
    /// A peer sent us a peer-to-peer message the node did not request.
    UnsolicitedMessage,
    /// The peer sent us a potential fork.
    EvaluatingFork,
    /// An unexpected error occurred processing a peer-to-peer message.
    UnexpectedSyncError {
        /// Additional context as to why block syncing failed.
        warning: String,
    },
    /// A channel that was supposed to receive a message was dropped.
    ChannelDropped,
}

impl core::fmt::Display for Warning {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Warning::NeedConnections {
                connected,
                required,
            } => {
                write!(
                    f,
                    "Looking for connections to peers. Connected: {connected}, Required: {required}"
                )
            }
            Warning::CouldNotConnect => {
                write!(f, "An attempted connection failed or timed out.")
            }
            Warning::NoCompactFilters => {
                write!(f, "A connected peer does not serve compact block filters.")
            }
            Warning::PotentialStaleTip => {
                write!(
                    f,
                    "The node has been running for a long duration without receiving new blocks."
                )
            }
            Warning::EvaluatingFork => write!(f, "Peer sent us a potential fork."),
            Warning::UnexpectedSyncError { warning } => {
                write!(f, "Error handling a P2P message: {warning}")
            }
            Warning::PeerTimedOut => {
                write!(f, "A connection to a peer timed out.")
            }
            Warning::UnsolicitedMessage => {
                write!(
                    f,
                    "A peer sent us a peer-to-peer message the node did not request."
                )
            }
            Warning::ChannelDropped => {
                write!(
                    f,
                    "A channel that was supposed to receive a message was dropped."
                )
            }
        }
    }
}
