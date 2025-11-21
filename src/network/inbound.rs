use bip324::serde::NetworkMessage;
use bip324::{PacketReader, PacketType};
use bitcoin::consensus::{deserialize, deserialize_partial};
use bitcoin::p2p::message::RawNetworkMessage;
use bitcoin::Network;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::error::ReaderError;
use super::V1Header;

const MAX_MESSAGE_BYTES: u32 = 1024 * 1024 * 32;

pub(crate) enum MessageParser<R: AsyncBufReadExt + Send + Sync + Unpin> {
    V2(R, PacketReader),
    V1(R, Network),
}

impl<R: AsyncBufReadExt + Send + Sync + Unpin> MessageParser<R> {
    pub async fn read_message(&mut self) -> Result<Option<NetworkMessage>, ReaderError> {
        match self {
            MessageParser::V2(stream, decryptor) => {
                let mut len_buf = [0; 3];
                let _ = stream.read_exact(&mut len_buf).await?;
                let message_len = decryptor.decypt_len(len_buf);
                if message_len > MAX_MESSAGE_BYTES as usize {
                    return Err(ReaderError::MessageTooLarge);
                }
                let mut response_message = vec![0; message_len];
                let _ = stream.read_exact(&mut response_message).await?;
                let msg = decryptor.decrypt_payload(&response_message, None)?;
                match msg.packet_type() {
                    PacketType::Genuine => {
                        let parsed = bip324::serde::deserialize(msg.contents())?;
                        Ok(Some(parsed))
                    }
                    PacketType::Decoy => Ok(None),
                }
            }
            MessageParser::V1(stream, network) => {
                let mut message_buf = vec![0_u8; 24];
                let _ = stream.read_exact(&mut message_buf).await?;
                let header: V1Header = deserialize_partial(&message_buf)?.0;
                // Nonsense for our network
                if header.magic != network.magic() {
                    return Err(ReaderError::InvalidDeserialization);
                }
                // Message is too long
                if header.length > MAX_MESSAGE_BYTES {
                    return Err(ReaderError::MessageTooLarge);
                }
                let mut contents_buf = vec![0_u8; header.length as usize];
                let _ = stream.read_exact(&mut contents_buf).await?;
                message_buf.extend_from_slice(&contents_buf);
                let message: RawNetworkMessage = deserialize(&message_buf)?;
                Ok(Some(message.into_payload()))
            }
        }
    }
}
