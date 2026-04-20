use std::fmt::Debug;

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
    /// No peer requested the transaction within the configured broadcast timeout.
    BroadcastTimeout,
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
            ClientError::BroadcastTimeout => {
                write!(
                    f,
                    "no peer requested the transaction within the configured timeout."
                )
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
