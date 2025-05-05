use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{SystemTime, UNIX_EPOCH},
};

use bip324::{PacketType, PacketWriter};
use bitcoin::{
    consensus::serialize,
    hashes::Hash,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetHeadersMessage, Inventory},
        message_filter::{GetCFHeaders, GetCFilters},
        message_network::VersionMessage,
        Address, ServiceFlags,
    },
    BlockHash, Network, Transaction, Wtxid,
};

use crate::{channel_messages::GetBlockConfig, prelude::default_port_from_network};

use super::{error::PeerError, KYOTO_VERSION, PROTOCOL_VERSION, RUST_BITCOIN_VERSION};

// Responsible for serializing messages to write over the wire, either encrypted or plaintext.
pub(crate) enum MessageGenerator {
    V1 {
        network: Network,
    },
    V2 {
        network: Network,
        encryptor: PacketWriter,
    },
}

impl MessageGenerator {
    pub(crate) fn version_message(&mut self, port: Option<u16>) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let msg = make_version(port, network);
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::Version(msg));
                Ok(serialize(&data))
            }
            MessageGenerator::V2 { network, encryptor } => {
                let msg = make_version(port, network);
                let plaintext = serialize_network_message(NetworkMessage::Version(msg))?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn verack(&mut self) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::Verack);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::Verack)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn addr(&mut self) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::GetAddr);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::GetAddr)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn addrv2(&mut self) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::SendAddrV2);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::SendAddrV2)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn wtxid_relay(&mut self) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::WtxidRelay);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::WtxidRelay)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn headers(
        &mut self,
        locator_hashes: Vec<BlockHash>,
        stop_hash: Option<BlockHash>,
    ) -> Result<Vec<u8>, PeerError> {
        let msg =
            GetHeadersMessage::new(locator_hashes, stop_hash.unwrap_or(BlockHash::all_zeros()));
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), NetworkMessage::GetHeaders(msg));
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::GetHeaders(msg))?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn cf_headers(&mut self, message: GetCFHeaders) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data =
                    RawNetworkMessage::new(network.magic(), NetworkMessage::GetCFHeaders(message));
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::GetCFHeaders(message))?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn filters(&mut self, message: GetCFilters) -> Result<Vec<u8>, PeerError> {
        match self {
            MessageGenerator::V1 { network } => {
                let data =
                    RawNetworkMessage::new(network.magic(), NetworkMessage::GetCFilters(message));
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::GetCFilters(message))?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn block(&mut self, config: GetBlockConfig) -> Result<Vec<u8>, PeerError> {
        let inv = get_block_from_cfg(config);
        match self {
            MessageGenerator::V1 { network } => {
                let data =
                    RawNetworkMessage::new(network.magic(), NetworkMessage::GetData(vec![inv]));
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(NetworkMessage::GetData(vec![inv]))?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn pong(&mut self, nonce: u64) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Pong(nonce);
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), msg);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(msg)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn announce_transaction(&mut self, wtxid: Wtxid) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Inv(vec![Inventory::WTx(wtxid)]);
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), msg);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(msg)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn broadcast_transaction(
        &mut self,
        transaction: Transaction,
    ) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Tx(transaction);
        match self {
            MessageGenerator::V1 { network } => {
                let data = RawNetworkMessage::new(network.magic(), msg);
                Ok(serialize(&data))
            }
            MessageGenerator::V2 {
                network: _,
                encryptor,
            } => {
                let plaintext = serialize_network_message(msg)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }
}

fn serialize_network_message(message: NetworkMessage) -> Result<Vec<u8>, PeerError> {
    bip324::serde::serialize(message).map_err(|_| PeerError::MessageSerialization)
}

fn encrypt_plaintext(
    encryptor: &mut PacketWriter,
    plaintext: Vec<u8>,
) -> Result<Vec<u8>, PeerError> {
    encryptor
        .encrypt_packet(&plaintext, None, PacketType::Genuine)
        .map_err(|_| PeerError::MessageEncryption)
}

fn make_version(port: Option<u16>, network: &Network) -> VersionMessage {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs();
    let default_port = default_port_from_network(network);
    let ip = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        port.unwrap_or(default_port),
    );
    let from_and_recv = Address::new(&ip, ServiceFlags::NONE);
    VersionMessage {
        version: PROTOCOL_VERSION,
        services: ServiceFlags::NONE,
        timestamp: now as i64,
        receiver: from_and_recv.clone(),
        sender: from_and_recv,
        nonce: 1,
        user_agent: format!(
            "Kyoto Light Client / {KYOTO_VERSION} / rust-bitcoin {RUST_BITCOIN_VERSION}"
        ),
        start_height: 0,
        relay: false,
    }
}

fn get_block_from_cfg(config: GetBlockConfig) -> Inventory {
    if cfg!(feature = "filter-control") {
        Inventory::WitnessBlock(config.locator)
    } else {
        Inventory::Block(config.locator)
    }
}
