extern crate alloc;
use crate::prelude::encode_qname;
use bitcoin::{
    key::rand::{thread_rng, RngCore},
    Network,
};
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};
use tokio::net::UdpSocket;

use super::error::DNSQueryError;

const SIGNET_SEEDS: &[&str; 2] = &["seed.dlsouza.lol", "seed.signet.bitcoin.sprovoost.nl"];

const TESTNET_SEEDS: &[&str; 4] = &[
    "testnet-seed.bitcoin.jonasschnelli.ch",
    "seed.tbtc.petertodd.org",
    "seed.testnet.bitcoin.sprovoost.nl",
    "testnet-seed.bluematt.me",
];

const MAINNET_SEEDS: &[&str; 9] = &[
    "seed.bitcoin.sipa.be",
    "dnsseed.bluematt.me",
    "dnsseed.bitcoin.dashjr.org",
    "seed.bitcoinstats.com",
    "seed.bitcoin.jonasschnelli.ch",
    "seed.btc.petertodd.org",
    "seed.bitcoin.sprovoost.nl",
    "dnsseed.emzy.de",
    "seed.bitcoin.wiz.biz",
];

const TESTNET4_SEEDS: &[&str; 2] = &[
    "seed.testnet4.bitcoin.sprovoost.nl",
    "seed.testnet4.wiz.biz",
];

pub(crate) const CBF_SERVICE_BIT_PREFIX: &str = "x49"; // Compact Filters, Node Network
pub(crate) const CBF_V2T_SERVICE_BIT_PREFIX: &str = "x849"; // Compact Filters, Node Network, P2P V2

const SERVICE_BITS_PREFIX: &[&str; 2] = &[CBF_SERVICE_BIT_PREFIX, CBF_V2T_SERVICE_BIT_PREFIX];

pub(crate) const DNS_RESOLVER_PORT: u16 = 53;
const LOCAL_HOST: &str = "0.0.0.0:0";

const HEADER_BYTES: usize = 12;

const RECURSIVE_FLAGS: [u8; 2] = [
    0x01, 0x00, // Default flags with recursive resolver
];

const QTYPE: [u8; 4] = [
    0x00, 0x01, // QType: A Record
    0x00, 0x01, // IN
];

const COUNTS: [u8; 6] = [
    0x00, 0x00, // ANCOUNT
    0x00, 0x00, // NSCOUNT
    0x00, 0x00, // ARCOUNT
];

const A_RECORD: u16 = 0x01;
const A_CLASS: u16 = 0x01;
const EXPECTED_RDATA_LEN: u16 = 0x04;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DnsResolver {
    pub(crate) socket_addr: SocketAddr,
}

impl Default for DnsResolver {
    fn default() -> Self {
        let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), DNS_RESOLVER_PORT);
        Self { socket_addr }
    }
}

impl From<DnsResolver> for SocketAddr {
    fn from(value: DnsResolver) -> Self {
        value.socket_addr
    }
}

pub(crate) async fn bootstrap_dns(network: Network, dns_resolver: DnsResolver) -> Vec<IpAddr> {
    let seeds = match network {
        Network::Bitcoin => MAINNET_SEEDS.to_vec(),
        Network::Testnet => TESTNET_SEEDS.to_vec(),
        Network::Signet => SIGNET_SEEDS.to_vec(),
        Network::Regtest => Vec::with_capacity(0),
        Network::Testnet4 => TESTNET4_SEEDS.to_vec(),
        _ => unreachable!(),
    };
    let mut ip_addrs: Vec<IpAddr> = vec![];
    for host in seeds {
        for filter in SERVICE_BITS_PREFIX {
            if let Ok(addrs) = DnsQuery::new(host, filter)
                .lookup(dns_resolver.into())
                .await
            {
                ip_addrs.extend(addrs);
            }
        }
    }
    ip_addrs
}

pub(crate) struct DnsQuery {
    message_id: [u8; 2],
    message: Vec<u8>,
    question: Vec<u8>,
}

impl DnsQuery {
    pub(crate) fn new(seed: &str, service_bit_prefix: &str) -> Self {
        // Build a header
        let mut rng = thread_rng();
        let mut message_id = [0, 0];
        rng.fill_bytes(&mut message_id);
        let mut message = message_id.to_vec();
        message.extend(RECURSIVE_FLAGS);
        message.push(0x00); // QDCOUNT
        message.push(0x01); // QDCOUNT
        message.extend(COUNTS);
        let mut question = encode_qname(seed, service_bit_prefix);
        question.extend(QTYPE);
        message.extend_from_slice(&question);
        Self {
            message_id,
            message,
            question,
        }
    }

    pub(crate) async fn lookup(
        &self,
        dns_resolver: SocketAddr,
    ) -> Result<Vec<IpAddr>, DNSQueryError> {
        let sock = UdpSocket::bind(LOCAL_HOST).await?;
        sock.connect(dns_resolver).await?;
        sock.send(&self.message).await?;
        sock.send(&self.message).await?;
        let mut response_buf = [0u8; 512];
        let (amt, _src) = sock.recv_from(&mut response_buf).await?;
        if amt < HEADER_BYTES {
            return Err(DNSQueryError::MalformedHeader);
        }
        let ips = self.parse_message(&response_buf[..amt])?;
        Ok(ips)
    }

    fn parse_message(&self, mut response: &[u8]) -> Result<Vec<IpAddr>, DNSQueryError> {
        let mut ips = Vec::with_capacity(10);
        let mut buf: [u8; 2] = [0, 0];
        response.read_exact(&mut buf)?; // Read 2 bytes
        if self.message_id != buf {
            return Err(DNSQueryError::MessageId);
        }
        // Read flags and ignore
        response.read_exact(&mut buf)?; // Read 4 bytes
        response.read_exact(&mut buf)?; // Read 6 bytes
        let _qdcount = u16::from_be_bytes(buf);
        response.read_exact(&mut buf)?; // Read 8 bytes
        let ancount = u16::from_be_bytes(buf);
        response.read_exact(&mut buf)?; // Read 10 bytes
        let _nscount = u16::from_be_bytes(buf);
        response.read_exact(&mut buf)?; // Read 12 bytes
        let _arcount = u16::from_be_bytes(buf);
        // The question should be repeated back to us
        let mut buf: Vec<u8> = vec![0; self.question.len()];
        response.read_exact(&mut buf)?;
        if self.question != buf {
            return Err(DNSQueryError::Question);
        }
        for _ in 0..ancount {
            let mut buf: [u8; 2] = [0, 0];
            // Read the compressed NAME field of the record and ignore
            response.read_exact(&mut buf)?;
            // Read the TYPE
            response.read_exact(&mut buf)?;
            let atype = u16::from_be_bytes(buf);
            // Read the CLASS
            response.read_exact(&mut buf)?;
            let aclass = u16::from_be_bytes(buf);
            let mut buf: [u8; 4] = [0, 0, 0, 0];
            // Read the TTL
            response.read_exact(&mut buf)?;
            let _ttl = u32::from_be_bytes(buf);
            let mut buf: [u8; 2] = [0, 0];
            // Read the RDLENGTH
            response.read_exact(&mut buf)?;
            let rdlength = u16::from_be_bytes(buf);
            // Read RDATA
            let mut rdata: Vec<u8> = vec![0; rdlength as usize];
            response.read_exact(&mut rdata)?;
            if atype == A_RECORD && aclass == A_CLASS && rdlength == EXPECTED_RDATA_LEN {
                ips.push(IpAddr::V4(Ipv4Addr::new(
                    rdata[0], rdata[1], rdata[2], rdata[3],
                )))
            }
        }
        Ok(ips)
    }
}
