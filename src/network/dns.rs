extern crate alloc;
use bitcoin::Network;
use std::net::IpAddr;

const SIGNET_SEEDS: &[&str; 3] = &[
    "seed.dlsouza.lol",
    "seed.signet.bitcoin.sprovoost.nl",
    "seed.signet.achownodes.xyz",
];

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

pub(crate) async fn bootstrap_dns(network: Network) -> Vec<IpAddr> {
    let seeds = match network {
        Network::Bitcoin => MAINNET_SEEDS.to_vec(),
        Network::Testnet => TESTNET_SEEDS.to_vec(),
        Network::Signet => SIGNET_SEEDS.to_vec(),
        Network::Regtest => Vec::with_capacity(0),
        Network::Testnet4 => TESTNET4_SEEDS.to_vec(),
    };
    let mut ip_addrs: Vec<IpAddr> = vec![];
    for host in seeds {
        let hosts = lookup_hostname(host).await;
        ip_addrs.extend(hosts);
    }
    ip_addrs
}

pub(crate) async fn lookup_hostname(host: &str) -> Vec<IpAddr> {
    let hostnames = [
        format!("{host}:{DNS_RESOLVER_PORT}"),
        format!("{}.{host}:{DNS_RESOLVER_PORT}", SERVICE_BITS_PREFIX[0]),
        format!("{}.{host}:{DNS_RESOLVER_PORT}", SERVICE_BITS_PREFIX[1]),
    ];
    let mut ip_addrs = Vec::new();
    for hostname in hostnames {
        if let Ok(peers) = tokio::net::lookup_host(hostname).await {
            ip_addrs.extend(peers.map(|ip| ip.ip()));
        }
    }
    ip_addrs
}
