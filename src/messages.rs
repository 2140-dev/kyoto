use std::collections::BTreeMap;

use bitcoin::p2p::address::AddrV2;
use bitcoin::p2p::ServiceFlags;
use bitcoin::Transaction;
use bitcoin::{block::Header, BlockHash, FeeRate, Wtxid};
use tokio::sync::mpsc::Receiver;

use crate::chain::{BlockHeaderChanges, IndexedHeader};
use crate::{chain::checkpoints::Checkpoint, IndexedBlock, TrustedPeer};
use crate::{BlockType, GossipMonitorRequest, IndexedFilter, Package};

use super::error::FetchBlockError;

/// Informational messages emitted by a node
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The chain of block headers has been altered in some way.
    ChainUpdate(BlockHeaderChanges),
    /// The node is fully synced, having scanned the requested range.
    FiltersSynced(SyncUpdate),
    /// A compact block filter with associated height and block hash.
    IndexedFilter(IndexedFilter),
}

/// The node has synced to a new tip of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncUpdate {
    // Last known tip of the blockchain
    tip: Checkpoint,
    // Ten recent headers ending with the tip
    recent_history: BTreeMap<u32, Header>,
}

impl SyncUpdate {
    pub(crate) fn new(tip: Checkpoint, recent_history: BTreeMap<u32, Header>) -> Self {
        Self {
            tip,
            recent_history,
        }
    }

    /// Get the tip of the blockchain after this sync.
    pub fn tip(&self) -> Checkpoint {
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
    BestBlock(ClientRequest<(), Checkpoint>),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// The node is looking for connections to peers.
    NeedConnections {
        /// The number of live connections.
        connected: usize,
        /// The configured requirement.
        required: usize,
    },
    /// A connection to a peer failed or was dropped.
    ConnectionFailed(ConnectionFailure),
    /// The node has been waiting for new `inv` and will find new peers to avoid block withholding.
    PotentialStaleTip,
    /// The peer sent us a potential fork.
    EvaluatingFork,
    /// A peer sent data that failed validation or that the node did not request.
    PeerMisbehaved(Misbehavior),
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
            Warning::ConnectionFailed(reason) => {
                write!(f, "A peer connection failed: {reason}")
            }
            Warning::PotentialStaleTip => {
                write!(
                    f,
                    "The node has been running for a long duration without receiving new blocks."
                )
            }
            Warning::EvaluatingFork => write!(f, "Peer sent us a potential fork."),
            Warning::PeerMisbehaved(misbehavior) => {
                write!(f, "A peer misbehaved: {misbehavior}")
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

/// The reasons a connection to a peer may fail or be dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ConnectionFailure {
    /// The node was unable to establish a connection or complete an encrypted handshake.
    CouldNotConnect,
    /// The peer did not respond to a request in time.
    TimedOut,
    /// The peer does not serve compact block filters.
    NoCompactFilters,
    /// The peer uses a peer-to-peer protocol version the node does not support.
    UnsupportedVersion,
}

impl core::fmt::Display for ConnectionFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConnectionFailure::CouldNotConnect => write!(f, "could not connect"),
            ConnectionFailure::TimedOut => write!(f, "peer timed out"),
            ConnectionFailure::NoCompactFilters => {
                write!(f, "peer does not serve compact block filters")
            }
            ConnectionFailure::UnsupportedVersion => {
                write!(f, "peer protocol version is not supported")
            }
        }
    }
}

/// The ways a peer may send data that fails validation or that the node did not request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Misbehavior {
    /// The peer sent block headers that failed validation.
    InvalidHeaders,
    /// The peer sent compact filter headers that conflict with those of another peer.
    ConflictingFilterHeaders,
    /// The peer sent compact filter headers that failed validation.
    InvalidFilterHeaders,
    /// The peer sent a compact block filter that failed validation.
    InvalidFilter,
    /// The peer sent a block whose header the node does not know.
    UnknownBlock,
    /// The peer sent a block with a merkle root that does not match its header.
    InvalidMerkleRoot,
    /// The peer sent a peer-to-peer message the node did not request.
    UnsolicitedMessage,
}

impl core::fmt::Display for Misbehavior {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Misbehavior::InvalidHeaders => write!(f, "invalid block headers"),
            Misbehavior::ConflictingFilterHeaders => {
                write!(f, "compact filter headers conflict with another peer")
            }
            Misbehavior::InvalidFilterHeaders => write!(f, "invalid compact filter headers"),
            Misbehavior::InvalidFilter => write!(f, "invalid compact block filter"),
            Misbehavior::UnknownBlock => write!(f, "block header is unknown"),
            Misbehavior::InvalidMerkleRoot => {
                write!(f, "block merkle root does not match its header")
            }
            Misbehavior::UnsolicitedMessage => write!(f, "unsolicited peer-to-peer message"),
        }
    }
}
