use crate::impl_sourceless_error;

#[derive(Debug)]
pub(crate) enum PeerReadError {
    ReadBuffer,
    Deserialization,
    DecryptionFailed,
    TooManyMessages,
    MpscChannel,
}

impl core::fmt::Display for PeerReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PeerReadError::ReadBuffer => write!(f, "reading bytes off the stream failed."),
            PeerReadError::Deserialization => {
                write!(f, "the message could not be properly deserialized.")
            }
            PeerReadError::TooManyMessages => write!(f, "DOS protection."),
            PeerReadError::MpscChannel => write!(f, "sending over the channel failed."),
            PeerReadError::DecryptionFailed => write!(f, "decrypting a message failed."),
        }
    }
}

impl_sourceless_error!(PeerReadError);

#[derive(Debug)]
pub(crate) enum PeerError {
    ConnectionFailed,
    MessageEncryption,
    MessageSerialization,
    HandshakeFailed,
    BufferWrite,
    ThreadChannel,
    DisconnectCommand,
    Reader,
    UnreachableSocketAddr,
    Socks5(Socks5Error),
}

impl core::fmt::Display for PeerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PeerError::ConnectionFailed => {
                write!(f, "the peer's TCP port was closed or we could not connect.")
            }
            PeerError::BufferWrite => write!(f, "a message could not be written to the peer."),
            PeerError::ThreadChannel => write!(
                f,
                "experienced an error sending a message over the channel."
            ),
            PeerError::DisconnectCommand => {
                write!(f, "the main thread advised this peer to disconnect.")
            }
            PeerError::Reader => write!(f, "the reading thread encountered an error."),
            PeerError::UnreachableSocketAddr => {
                write!(f, "cannot make use of provided p2p address.")
            }
            PeerError::MessageSerialization => {
                write!(f, "serializing a message into bytes failed.")
            }
            PeerError::HandshakeFailed => {
                write!(f, "an attempted V2 transport handshake failed.")
            }
            PeerError::MessageEncryption => {
                write!(f, "encrypting a serialized message failed.")
            }
            PeerError::Socks5(err) => {
                write!(f, "could not connect via Socks5 proxy: {err}")
            }
        }
    }
}

impl_sourceless_error!(PeerError);

#[derive(Debug, Clone)]
pub(crate) enum Socks5Error {
    WrongVersion,
    AuthRequired,
    ConnectionTimeout,
    ConnectionFailed,
    IO,
}

impl core::fmt::Display for Socks5Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Socks5Error::WrongVersion => write!(f, "server responded with an unsupported version."),
            Socks5Error::AuthRequired => write!(f, "server requires authentication."),
            Socks5Error::ConnectionTimeout => write!(f, "connection to server timed out."),
            Socks5Error::ConnectionFailed => write!(
                f,
                "the server could not connect to the requested destination."
            ),
            Socks5Error::IO => write!(
                f,
                "reading or writing to the TCP stream failed unexpectedly."
            ),
        }
    }
}

impl_sourceless_error!(Socks5Error);

impl From<std::io::Error> for Socks5Error {
    fn from(_value: std::io::Error) -> Self {
        Socks5Error::IO
    }
}

#[derive(Debug)]
pub(crate) enum DNSQueryError {
    MessageID,
    Question,
    ConnectionDenied,
    Udp,
    MalformedHeader,
    UnexpectedEOF,
}

impl core::fmt::Display for DNSQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DNSQueryError::ConnectionDenied => write!(f, "the UDP connection failed."),
            DNSQueryError::MalformedHeader => write!(f, "the DNS response header was too short."),
            DNSQueryError::UnexpectedEOF => {
                write!(f, "the end of the response was reached before we expected.")
            }
            DNSQueryError::Udp => write!(f, "reading or writing from the UDP connection failed."),
            DNSQueryError::MessageID => write!(f, "mismatch of message ID."),
            DNSQueryError::Question => write!(f, "the question of the message does not match."),
        }
    }
}

impl_sourceless_error!(DNSQueryError);
