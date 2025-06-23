use std::{collections::BTreeMap, ops::Range, time::Duration};

use bitcoin::{
    block::Header, p2p::message_network::RejectReason, BlockHash, FeeRate, ScriptBuf, Wtxid,
};
use tokio::sync::oneshot;

#[cfg(feature = "filter-control")]
use crate::IndexedFilter;
use crate::{
    chain::{checkpoints::HeaderCheckpoint, IndexedHeader},
    IndexedBlock, NodeState, TrustedPeer, TxBroadcast,
};

use super::error::{FetchBlockError, FetchHeaderError};

/// Informational messages emitted by a node
#[derive(Debug, Clone)]
pub enum Info {
    /// The current state of the node in the syncing process.
    StateChange(NodeState),
    /// The node was able to successfully complete a version handshake.
    SuccessfulHandshake,
    /// The node is connected to all required peers.
    ConnectionsMet,
    /// The progress of the node during the block filter download process.
    Progress(Progress),
    /// There was an update to the header chain.
    NewChainHeight(u32),
    /// A peer served a new fork.
    NewFork {
        /// The tip of the new fork.
        tip: IndexedHeader,
    },
    /// A transaction was sent to a peer. The `wtxid` was advertised to the
    /// peer, and the peer responded with `getdata`. The transaction was then serialized and sent
    /// over the wire. This is a strong indication the transaction will propagate, but not
    /// guaranteed. You may receive duplicate messages for a given `wtxid` given your broadcast
    /// policy.
    TxGossiped(Wtxid),
}

impl core::fmt::Display for Info {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Info::StateChange(s) => write!(f, "{s}"),
            Info::SuccessfulHandshake => write!(f, "Successful version handshake with a peer"),
            Info::TxGossiped(txid) => write!(f, "Transaction gossiped: {txid}"),
            Info::ConnectionsMet => write!(f, "Required connections met"),
            Info::Progress(p) => {
                let progress_percent = p.percentage_complete();
                write!(f, "Percent complete: {progress_percent}")
            }
            Info::NewChainHeight(height) => write!(f, "New chain height: {height}"),
            Info::NewFork { tip } => write!(f, "New fork {} -> {}", tip.height, tip.block_hash()),
        }
    }
}

/// Data and structures useful for a consumer, such as a wallet.
#[derive(Debug, Clone)]
pub enum Event {
    /// A relevant [`Block`](crate) based on the user provided scripts.
    /// Note that the block may not contain any transactions contained in the script set.
    /// This is due to block filters having a non-zero false-positive rate when compressing data.
    Block(IndexedBlock),
    /// The node is fully synced, having scanned the requested range.
    Synced(SyncUpdate),
    /// Blocks were reorganized out of the chain.
    BlocksDisconnected {
        /// Blocks that were accepted to the chain of most work in ascending order by height.
        accepted: Vec<IndexedHeader>,
        /// Blocks that were disconnected from the chain of most work in ascending order by height.
        disconnected: Vec<IndexedHeader>,
    },
    /// A compact block filter with associated height and block hash.
    #[cfg(feature = "filter-control")]
    IndexedFilter(IndexedFilter),
}

/// The node has synced to a new tip of the chain.
#[derive(Debug, Clone)]
pub struct SyncUpdate {
    /// Last known tip of the blockchain
    pub tip: HeaderCheckpoint,
    /// Ten recent headers ending with the tip
    pub recent_history: BTreeMap<u32, Header>,
}

impl SyncUpdate {
    pub(crate) fn new(tip: HeaderCheckpoint, recent_history: BTreeMap<u32, Header>) -> Self {
        Self {
            tip,
            recent_history,
        }
    }

    /// Get the tip of the blockchain after this sync.
    pub fn tip(&self) -> HeaderCheckpoint {
        self.tip
    }

    /// Get the ten most recent blocks in chronological order after this sync.
    pub fn recent_history(&self) -> &BTreeMap<u32, Header> {
        &self.recent_history
    }
}

/// The progress of the node during the block filter download process.

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Progress {
    /// The number of filter headers that have been assumed checked and downloaded.
    pub filter_headers: u32,
    /// The number of block filters that have been assumed checked and downloaded.
    pub filters: u32,
    /// The number of filters to check.
    pub total_to_check: u32,
}

impl Progress {
    pub(crate) fn new(filter_headers: u32, filters: u32, total_to_check: u32) -> Self {
        Self {
            filter_headers,
            filters,
            total_to_check,
        }
    }

    /// The total progress represented as a percent.
    pub fn percentage_complete(&self) -> f32 {
        self.fraction_complete() * 100.0
    }

    /// The total progress represented as a fraction.
    pub fn fraction_complete(&self) -> f32 {
        let total = (2 * self.total_to_check) as f32;
        (self.filter_headers + self.filters) as f32 / total
    }
}

/// An attempt to broadcast a transaction failed.
#[derive(Debug, Clone, Copy)]
pub struct RejectPayload {
    /// An enumeration of the reason for the transaction failure. If none is provided, the message could not be sent over the wire.
    pub reason: Option<RejectReason>,
    /// The transaction that was rejected or failed to broadcast.
    pub wtxid: Wtxid,
}

/// Commands to issue a node.
#[derive(Debug)]
pub(crate) enum ClientMessage {
    /// Stop the node.
    Shutdown,
    /// Broadcast a [`crate::Transaction`] with a [`crate::TxBroadcastPolicy`].
    Broadcast(TxBroadcast),
    /// Add more Bitcoin [`ScriptBuf`] to look for.
    #[allow(dead_code)]
    AddScript(ScriptBuf),
    /// Starting at the configured anchor checkpoint, look for block inclusions with newly added scripts.
    Rescan,
    /// Explicitly request a block from the node.
    GetBlock(BlockRequest),
    /// Fetch the fee rates for the given block hash.
    FetchFees(FeeRequest),
    /// Set a new connection timeout.
    SetDuration(Duration),
    /// Add another known peer to connect to.
    AddPeer(TrustedPeer),
    /// Request a header from a specified height.
    GetHeader(HeaderRequest),
    /// Request a range of headers.
    GetHeaderBatch(BatchHeaderRequest),
    /// Request the broadcast minimum fee rate.
    GetBroadcastMinFeeRate(FeeRateSender),
    /// Send an empty message to see if the node is running.
    NoOp,
}

type HeaderSender = tokio::sync::oneshot::Sender<Result<Header, FetchHeaderError>>;

#[derive(Debug)]
pub(crate) struct HeaderRequest {
    pub(crate) oneshot: HeaderSender,
    pub(crate) height: u32,
}

impl HeaderRequest {
    pub(crate) fn new(oneshot: HeaderSender, height: u32) -> Self {
        Self { oneshot, height }
    }
}

type BatchHeaderSender =
    tokio::sync::oneshot::Sender<Result<BTreeMap<u32, Header>, FetchHeaderError>>;

#[derive(Debug)]
pub(crate) struct BatchHeaderRequest {
    pub(crate) oneshot: BatchHeaderSender,
    pub(crate) range: Range<u32>,
}

impl BatchHeaderRequest {
    pub(crate) fn new(oneshot: BatchHeaderSender, range: Range<u32>) -> Self {
        Self { oneshot, range }
    }
}

pub(crate) type FeeRateSender = tokio::sync::oneshot::Sender<FeeRate>;

#[derive(Debug)]
pub(crate) struct BlockRequest {
    pub(crate) oneshot: oneshot::Sender<Result<IndexedBlock, FetchBlockError>>,
    pub(crate) hash: BlockHash,
}

impl BlockRequest {
    pub(crate) fn new(
        oneshot: oneshot::Sender<Result<IndexedBlock, FetchBlockError>>,
        hash: BlockHash,
    ) -> Self {
        Self { oneshot, hash }
    }
}

#[derive(Debug)]
pub(crate) struct FeeRequest {
    pub(crate) oneshot: oneshot::Sender<Result<Vec<FeeRate>, FetchBlockError>>,
    pub(crate) hash: BlockHash,
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
    /// The provided anchor is deeper than the database history.
    /// Recoverable by deleting the headers from the database or starting from a higher point in the chain.
    InvalidStartHeight,
    /// The headers in the database do not link together. Recoverable by deleting the database.
    CorruptedHeaders,
    /// A transaction got rejected, likely for being an insufficient fee or non-standard transaction.
    TransactionRejected {
        /// The transaction ID and reject reason, if it exists.
        payload: RejectPayload,
    },
    /// A database failed to persist some data.
    FailedPersistence {
        /// Additional context for the persistence failure.
        warning: String,
    },
    /// The peer sent us a potential fork.
    EvaluatingFork,
    /// The peer database has no values.
    EmptyPeerDatabase,
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
            Warning::InvalidStartHeight => write!(
                f,
                "The provided anchor is deeper than the database history."
            ),
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
            Warning::TransactionRejected { payload } => {
                write!(f, "A transaction got rejected: WTXID {}", payload.wtxid)
            }
            Warning::FailedPersistence { warning } => {
                write!(f, "A database failed to persist some data: {warning}")
            }
            Warning::EvaluatingFork => write!(f, "Peer sent us a potential fork."),
            Warning::EmptyPeerDatabase => write!(f, "The peer database has no values."),
            Warning::UnexpectedSyncError { warning } => {
                write!(f, "Error handling a P2P message: {warning}")
            }
            Warning::CorruptedHeaders => {
                write!(f, "The headers in the database do not link together.")
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
