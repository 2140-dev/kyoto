use std::collections::BTreeMap;
use std::ops::Div;

use bitcoin::{
    block::Header, p2p::message_network::RejectReason, BlockHash, FeeRate, Transaction, Wtxid,
};

use crate::chain::BlockHeaderChanges;
use crate::IndexedFilter;
use crate::{chain::checkpoints::HeaderCheckpoint, IndexedBlock, TrustedPeer};

use super::error::FetchBlockError;

/// Informational messages emitted by a node
#[derive(Debug, Clone)]
pub enum Info {
    /// The node was able to successfully complete a version handshake.
    SuccessfulHandshake,
    /// The node is connected to all required peers.
    ConnectionsMet,
    /// The progress of the node during the block filter download process.
    Progress(Progress),
    /// A requested block has been received and is being processed.
    BlockReceived(BlockHash),
}

impl core::fmt::Display for Info {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Info::SuccessfulHandshake => write!(f, "Successful version handshake with a peer"),
            Info::ConnectionsMet => write!(f, "Required connections met"),
            Info::Progress(p) => {
                let progress_percent = p.percentage_complete();
                write!(f, "Percent complete: {progress_percent}")
            }
            Info::BlockReceived(hash) => write!(f, "Received block {hash}"),
        }
    }
}

/// Data and structures useful for a consumer, such as a wallet.
#[derive(Debug, Clone)]
pub enum Event {
    /// A [`Block`](crate) that was requested.
    Block(IndexedBlock),
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
    // The number of filter headers that have been assumed checked and downloaded.
    filter_headers: u32,
    // The number of block filters that have been assumed checked and downloaded.
    filters: u32,
    // The number of filters to check.
    total_to_check: u32,
    // The total number of headers.
    chain_height: u32,
}

impl Progress {
    pub(crate) fn new(
        filter_headers: u32,
        filters: u32,
        total_to_check: u32,
        chain_height: u32,
    ) -> Self {
        Self {
            filter_headers,
            filters,
            total_to_check,
            chain_height,
        }
    }

    /// The height of the block chain.
    pub fn chain_height(&self) -> u32 {
        self.chain_height
    }

    /// The total progress represented as a percent.
    pub fn percentage_complete(&self) -> f32 {
        self.fraction_complete() * 100.0
    }

    /// The total progress represented as a fraction.
    pub fn fraction_complete(&self) -> f32 {
        // This is a weighted estimated. Here we assume filters are 3x more expensive to download.
        let total = (4 * self.total_to_check) as f32;
        ((self.filter_headers + 3 * self.filters) as f32).div(total)
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
    Broadcast(ClientRequest<Transaction, Wtxid>),
    /// Starting at the configured anchor checkpoint, re-emit all filters.
    Rescan,
    /// Explicitly request a block from the node.
    GetBlock(ClientRequest<BlockHash, Result<IndexedBlock, FetchBlockError>>),
    /// Add another known peer to connect to.
    AddPeer(TrustedPeer),
    /// Request the broadcast minimum fee rate.
    GetBroadcastMinFeeRate(ClientRequest<(), FeeRate>),
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
    /// A transaction got rejected, likely for being an insufficient fee or non-standard transaction.
    TransactionRejected {
        /// The transaction ID and reject reason, if it exists.
        payload: RejectPayload,
    },
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
            Warning::TransactionRejected { payload } => {
                write!(f, "A transaction got rejected: WTXID {}", payload.wtxid)
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
