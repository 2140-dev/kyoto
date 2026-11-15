use std::fmt::Debug;

use bitcoin::BlockHash;

use crate::impl_sourceless_error;

/// Errors that prevent the node from running.
#[derive(Debug)]
pub enum NodeError {
    /// The node has exhausted all possible options for peers.
    NoReachablePeers,
}

impl core::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::NoReachablePeers => {
                write!(f, "the node has exhausted all possible options for peers")
            }
        }
    }
}

impl_sourceless_error!(NodeError);

/// Errors occurring when the client is talking to the node.
#[derive(Debug)]
pub enum ClientError {
    /// The channel to the node was likely closed and dropped from memory.
    SendError,
    /// A channel was dropped before sending its value back.
    RecvError,
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::SendError => {
                write!(f, "the receiver of this message was dropped from memory.")
            }
            ClientError::RecvError => {
                write!(f, "the sender of data was dropped from memory.")
            }
        }
    }
}

impl_sourceless_error!(ClientError);

/// Errors occurring when the client is fetching blocks from the node.
#[derive(Debug)]
pub enum FetchBlockError {
    /// The channel to the node was likely closed and dropped from memory.
    /// This implies the node is not running.
    SendError,
    /// The channel to the client was likely closed by the node and dropped from memory.
    RecvError,
    /// The hash is not a member of the chain of most work.
    UnknownHash,
    /// The witness data of the block does not match the commitment in the coinbase.
    InvalidWitnessCommitment,
    /// The block was reorganized out of the chain of most work while the request was pending.
    Reorged(BlockHash),
}

impl core::fmt::Display for FetchBlockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchBlockError::SendError => {
                write!(f, "the receiver of this message was dropped from memory.")
            }
            FetchBlockError::RecvError => write!(
                f,
                "the channel to the client was likely closed by the node and dropped from memory."
            ),
            FetchBlockError::UnknownHash => {
                write!(f, "the hash is not a member of the chain of most work.")
            }
            FetchBlockError::InvalidWitnessCommitment => {
                write!(
                    f,
                    "the block witness data does not match the coinbase commitment."
                )
            }
            FetchBlockError::Reorged(hash) => {
                write!(
                    f,
                    "block {hash} was reorganized out of the chain of most work while pending."
                )
            }
        }
    }
}

impl_sourceless_error!(FetchBlockError);

/// Errors when constructing transaction packages.
#[derive(Debug)]
pub enum PackageError {
    /// Packages may not include more than two transactions and must include at least one
    /// transaction.
    InvalidPackageLength(usize),
    /// Child transactions must spend an output from the parent.
    UnrelatedTransactions,
}

impl core::fmt::Display for PackageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackageError::InvalidPackageLength(s) => {
                write!(
                    f,
                    "package must include at most two transactions, got {}",
                    s
                )
            }
            PackageError::UnrelatedTransactions => {
                write!(f, "packages must have dependent inputs and outputs.")
            }
        }
    }
}
