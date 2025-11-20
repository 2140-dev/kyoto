use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{SystemTime, UNIX_EPOCH},
};

use bip324::{PacketType, PacketWriter};
use bitcoin::{
    consensus::serialize,
    p2p::{
        message::{NetworkMessage, RawNetworkMessage},
        message_blockdata::{GetHeadersMessage, Inventory},
        message_filter::{GetCFHeaders, GetCFilters},
        message_network::VersionMessage,
        Address, ServiceFlags,
    },
    BlockHash, Network, Transaction, Wtxid,
};

use crate::default_port_from_network;

use super::{error::PeerError, KYOTO_VERSION, PROTOCOL_VERSION, RUST_BITCOIN_VERSION};

// Responsible for serializing messages to write over the wire, either encrypted or plaintext.
pub(crate) struct MessageGenerator {
    pub network: Network,
    pub transport: Transport,
}

pub(crate) enum Transport {
    V1,
    V2 { encryptor: PacketWriter },
}

impl MessageGenerator {
    fn serialize(&mut self, msg: NetworkMessage) -> Result<Vec<u8>, PeerError> {
        match &mut self.transport {
            Transport::V1 => {
                let data = RawNetworkMessage::new(self.network.magic(), msg);
                Ok(serialize(&data))
            }
            Transport::V2 { encryptor } => {
                let plaintext = serialize_network_message(msg)?;
                encrypt_plaintext(encryptor, plaintext)
            }
        }
    }

    pub(crate) fn version_message(&mut self, port: Option<u16>) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Version(make_version(port, &self.network));
        self.serialize(msg)
    }

    pub(crate) fn verack(&mut self) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Verack;
        self.serialize(msg)
    }

    pub(crate) fn addr(&mut self) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::GetAddr;
        self.serialize(msg)
    }

    pub(crate) fn addrv2(&mut self) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::SendAddrV2;
        self.serialize(msg)
    }

    pub(crate) fn wtxid_relay(&mut self) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::WtxidRelay;
        self.serialize(msg)
    }

    pub(crate) fn sendheaders(&mut self) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::SendHeaders;
        self.serialize(msg)
    }

    pub(crate) fn headers(&mut self, msg: GetHeadersMessage) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::GetHeaders(msg);
        self.serialize(msg)
    }

    pub(crate) fn cf_headers(&mut self, message: GetCFHeaders) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::GetCFHeaders(message);
        self.serialize(msg)
    }

    pub(crate) fn filters(&mut self, message: GetCFilters) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::GetCFilters(message);
        self.serialize(msg)
    }

    pub(crate) fn block(&mut self, hash: BlockHash) -> Result<Vec<u8>, PeerError> {
        let inv = Inventory::Block(hash);
        let msg = NetworkMessage::GetData(vec![inv]);
        self.serialize(msg)
    }

    pub(crate) fn ping(&mut self, nonce: u64) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Ping(nonce);
        self.serialize(msg)
    }

    pub(crate) fn pong(&mut self, nonce: u64) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Pong(nonce);
        self.serialize(msg)
    }

    pub(crate) fn announce_transactions(
        &mut self,
        wtxids: Vec<Wtxid>,
    ) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Inv(wtxids.into_iter().map(Inventory::WTx).collect());
        self.serialize(msg)
    }

    pub(crate) fn broadcast_transaction(
        &mut self,
        transaction: Transaction,
    ) -> Result<Vec<u8>, PeerError> {
        let msg = NetworkMessage::Tx(transaction);
        self.serialize(msg)
    }
}

fn serialize_network_message(message: NetworkMessage) -> Result<Vec<u8>, PeerError> {
    bip324::serde::serialize(message).map_err(From::from)
}

fn encrypt_plaintext(
    encryptor: &mut PacketWriter,
    plaintext: Vec<u8>,
) -> Result<Vec<u8>, PeerError> {
    encryptor
        .encrypt_packet(&plaintext, None, PacketType::Genuine)
        .map_err(From::from)
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
        user_agent: format!("/Rust BIP-157:{KYOTO_VERSION}/rust-bitcoin:{RUST_BITCOIN_VERSION}/"),
        start_height: 0,
        relay: false,
    }
}
