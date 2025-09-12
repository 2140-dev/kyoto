use std::fmt::{Debug, Display};

use crate::{db::error::SqlHeaderStoreError, impl_sourceless_error};

/// Errors that prevent the node from running.
#[derive(Debug)]
pub enum NodeError<P: Debug + Display> {
    /// The persistence layer experienced a critical error.
    PeerDatabase(PeerManagerError<P>),
}

impl<P: Debug + Display> core::fmt::Display for NodeError<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::PeerDatabase(e) => write!(f, "peer manager: {e}"),
        }
    }
}

impl<P: Debug + Display> std::error::Error for NodeError<P> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl<P: Debug + Display> From<PeerManagerError<P>> for NodeError<P> {
    fn from(value: PeerManagerError<P>) -> Self {
        NodeError::PeerDatabase(value)
    }
}

/// Errors when managing persisted peers.
#[derive(Debug)]
pub enum PeerManagerError<P: Debug + Display> {
    /// Reading or writing from the database failed.
    Database(P),
}

impl<P: Debug + Display> core::fmt::Display for PeerManagerError<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerManagerError::Database(e) => {
                write!(f, "database: {e}")
            }
        }
    }
}

impl<P: Debug + Display> std::error::Error for PeerManagerError<P> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

impl<P: Debug + Display> From<P> for PeerManagerError<P> {
    fn from(value: P) -> Self {
        PeerManagerError::Database(value)
    }
}

/// Errors with the block header representation that prevent the node from operating.
#[derive(Debug)]
pub enum HeaderPersistenceError {
    /// The block headers do not point to each other in a list.
    HeadersDoNotLink,
    /// Some predefined checkpoint does not match.
    MismatchedCheckpoints,
    /// A user tried to retrieve headers too far in the past for what is in their database.
    CannotLocateHistory,
    /// A database error.
    Database(SqlHeaderStoreError),
}

impl core::fmt::Display for HeaderPersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderPersistenceError::HeadersDoNotLink => write!(f, "the headers loaded from persistence do not link together."),
            HeaderPersistenceError::MismatchedCheckpoints => write!(f, "the headers loaded do not match a known checkpoint."),
            HeaderPersistenceError::CannotLocateHistory => write!(f, "the configured checkpoint is too far in the past compared to previous syncs. The database cannot reconstruct the chain."),
            HeaderPersistenceError::Database(e) => write!(f, "database: {e}"),
        }
    }
}

impl_sourceless_error!(HeaderPersistenceError);

/// Errors occurring when the client is talking to the node.
#[derive(Debug)]
pub enum ClientError {
    /// The channel to the node was likely closed and dropped from memory.
    SendError,
}

impl core::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::SendError => {
                write!(f, "the receiver of this message was dropped from memory.")
            }
        }
    }
}

impl_sourceless_error!(ClientError);

/// Errors occurring when the client is fetching headers from the node.
#[derive(Debug)]
pub enum FetchHeaderError {
    /// The channel to the node was likely closed and dropped from memory.
    /// This implies the node is not running.
    SendError,
    /// The channel to the client was likely closed by the node and dropped from memory.
    RecvError,
    /// The header at the requested height does not yet exist.
    UnknownHeight,
}

impl core::fmt::Display for FetchHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchHeaderError::SendError => {
                write!(f, "the receiver of this message was dropped from memory.")
            }
            FetchHeaderError::RecvError => write!(
                f,
                "the channel to the client was likely closed by the node and dropped from memory."
            ),
            FetchHeaderError::UnknownHeight => {
                write!(f, "the header at the requested height does not yet exist.")
            }
        }
    }
}

impl_sourceless_error!(FetchHeaderError);

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
        }
    }
}

impl_sourceless_error!(FetchBlockError);

/// Errors that occur when fetching the minimum fee rate to broadcast a transaction.
#[derive(Debug)]
pub enum FetchFeeRateError {
    /// The channel to the node was likely closed and dropped from memory.
    /// This implies the node is not running.
    SendError,
    /// The channel to the client was likely closed by the node and dropped from memory.
    RecvError,
}

impl core::fmt::Display for FetchFeeRateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchFeeRateError::SendError => {
                write!(f, "the receiver of this message was dropped from memory.")
            }
            FetchFeeRateError::RecvError => write!(
                f,
                "the channel to the client was likely closed by the node and dropped from memory."
            ),
        }
    }
}

impl_sourceless_error!(FetchFeeRateError);
