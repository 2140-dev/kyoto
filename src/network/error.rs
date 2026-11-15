use crate::impl_sourceless_error;

use bip324::io::ProtocolError;
use bip324::serde;
use bitcoin::consensus::encode;
use tokio::io;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(crate) enum ReaderError {
    Io(io::Error),
    InvalidDeserialization,
    DecryptionFailed(bip324::Error),
    MessageTooLarge,
    ChannelClosed,
}

impl core::fmt::Display for ReaderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReaderError::Io(err) => write!(f, "reading bytes off the stream failed: {err}"),
            ReaderError::InvalidDeserialization => {
                write!(f, "the message could not be properly deserialized")
            }
            ReaderError::MessageTooLarge => write!(f, "OOM protection"),
            ReaderError::ChannelClosed => write!(f, "sending over the channel failed"),
            ReaderError::DecryptionFailed(err) => write!(f, "decrypting a message failed: {err}"),
        }
    }
}

impl_sourceless_error!(ReaderError);

impl<T> From<mpsc::error::SendError<T>> for ReaderError {
    fn from(_value: mpsc::error::SendError<T>) -> Self {
        Self::ChannelClosed
    }
}

impl From<io::Error> for ReaderError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<encode::Error> for ReaderError {
    fn from(_value: encode::Error) -> Self {
        Self::InvalidDeserialization
    }
}

impl From<serde::Error> for ReaderError {
    fn from(_value: serde::Error) -> Self {
        Self::InvalidDeserialization
    }
}

impl From<bip324::Error> for ReaderError {
    fn from(value: bip324::Error) -> Self {
        Self::DecryptionFailed(value)
    }
}

impl From<ProtocolError> for ReaderError {
    fn from(value: ProtocolError) -> Self {
        match value {
            ProtocolError::Io(err, _) => Self::Io(err),
            ProtocolError::Internal(err) => Self::DecryptionFailed(err),
        }
    }
}

#[derive(Debug)]
pub(crate) enum PeerError {
    ConnectionFailed,
    Encryption(bip324::Error),
    Serialization(serde::Error),
    HandshakeFailed,
    Io(io::Error),
    ChannelClosed,
    DisconnectCommand,
    UnreachableSocketAddr,
    Socks5(Socks5Error),
}

impl core::fmt::Display for PeerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PeerError::ConnectionFailed => {
                write!(f, "the peer's TCP port was closed or we could not connect")
            }
            PeerError::Io(err) => write!(f, "a message could not be written to the peer: {err}"),
            PeerError::ChannelClosed => {
                write!(f, "experienced an error sending a message over the channel")
            }
            PeerError::DisconnectCommand => {
                write!(f, "the main thread advised this peer to disconnect")
            }
            PeerError::UnreachableSocketAddr => {
                write!(f, "cannot make use of provided p2p address")
            }
            PeerError::Serialization(err) => {
                write!(f, "serializing a message into bytes failed: {err}")
            }
            PeerError::HandshakeFailed => {
                write!(f, "an attempted V2 transport handshake failed")
            }
            PeerError::Encryption(err) => {
                write!(f, "encrypting a serialized message failed: {err}")
            }
            PeerError::Socks5(err) => {
                write!(f, "could not connect via Socks5 proxy: {err}")
            }
        }
    }
}

impl_sourceless_error!(PeerError);

impl From<io::Error> for PeerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl<T> From<mpsc::error::SendError<T>> for PeerError {
    fn from(_value: mpsc::error::SendError<T>) -> Self {
        Self::ChannelClosed
    }
}

impl From<bip324::Error> for PeerError {
    fn from(value: bip324::Error) -> Self {
        Self::Encryption(value)
    }
}

impl From<serde::Error> for PeerError {
    fn from(value: serde::Error) -> Self {
        Self::Serialization(value)
    }
}

#[derive(Debug)]
pub(crate) enum Socks5Error {
    WrongVersion,
    AuthRequired,
    ConnectionTimeout,
    ConnectionFailed,
    Io(io::Error),
}

impl core::fmt::Display for Socks5Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Socks5Error::WrongVersion => write!(f, "server responded with an unsupported version"),
            Socks5Error::AuthRequired => write!(f, "server requires authentication"),
            Socks5Error::ConnectionTimeout => write!(f, "connection to server timed out"),
            Socks5Error::ConnectionFailed => write!(
                f,
                "the server could not connect to the requested destination"
            ),
            Socks5Error::Io(err) => write!(f, "socks5 io failed unexpectedly: {err}"),
        }
    }
}

impl_sourceless_error!(Socks5Error);

impl From<std::io::Error> for Socks5Error {
    fn from(value: std::io::Error) -> Self {
        Socks5Error::Io(value)
    }
}
