use std::{
    collections::HashMap,
    fmt::Debug,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use addrman::Record;
use bitcoin::{
    key::rand,
    p2p::{address::AddrV2, ServiceFlags},
    FeeRate, Network,
};
use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};
use tokio::{
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    task::JoinHandle,
};

use crate::{
    broadcaster::BroadcastQueue,
    default_port_from_network,
    network::{dns::bootstrap_dns, error::PeerError, peer::Peer, PeerId, PeerTimeoutConfig},
    BlockType, Dialog, TrustedPeer, TrustedPeerInner,
};

use super::{AddressBook, ConnectionType, MainThreadMessage, PeerThreadMessage};

const LOCAL_HOST: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

// Preferred peers to connect to based on the user configuration
type Whitelist = Vec<TrustedPeer>;

// A peer that is or was connected to the node
#[derive(Debug)]
pub(crate) struct ManagedPeer {
    record: Record,
    broadcast_min: FeeRate,
    ptx: Sender<MainThreadMessage>,
    handle: JoinHandle<Result<(), PeerError>>,
}

// The `PeerMap` manages connections with peers, adds and bans peers, and manages the peer database
#[derive(Debug)]
pub(crate) struct PeerMap {
    pub(crate) tx_queue: Arc<Mutex<BroadcastQueue>>,
    pub(crate) whitelist_only: bool,
    current_id: PeerId,
    network: Network,
    block_type: BlockType,
    mtx: Sender<PeerThreadMessage>,
    map: HashMap<PeerId, ManagedPeer>,
    db: Arc<Mutex<AddressBook>>,
    connector: ConnectionType,
    whitelist: Whitelist,
    dialog: Arc<Dialog>,
    timeout_config: PeerTimeoutConfig,
}

impl PeerMap {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mtx: Sender<PeerThreadMessage>,
        network: Network,
        block_type: BlockType,
        whitelist: Whitelist,
        whitelist_only: bool,
        dialog: Arc<Dialog>,
        connection_type: ConnectionType,
        timeout_config: PeerTimeoutConfig,
    ) -> Self {
        Self {
            tx_queue: Arc::new(Mutex::new(BroadcastQueue::new())),
            whitelist_only,
            current_id: PeerId(0),
            network,
            block_type,
            mtx,
            map: HashMap::new(),
            db: Arc::new(Mutex::new(AddressBook::new())),
            connector: connection_type,
            whitelist,
            dialog,
            timeout_config,
        }
    }

    // Remove any finished connections
    pub async fn clean(&mut self) {
        self.map.retain(|_, peer| !peer.handle.is_finished());
    }

    // The number of peers with live connections
    pub fn live(&self) -> usize {
        self.map
            .values()
            .filter(|peer| !peer.handle.is_finished())
            .count()
    }

    // Add a new trusted peer to the whitelist
    pub fn add_trusted_peer(&mut self, peer: TrustedPeer) {
        self.whitelist.push(peer);
    }

    // In whitelist-only mode a non-empty whitelist is retried rather than ending the node.
    pub fn will_retry_whitelist(&self) -> bool {
        self.whitelist_only && !self.whitelist.is_empty()
    }

    // Send out a TCP connection to a new peer and begin tracking the task
    pub async fn dispatch(&mut self, loaded_peer: Record) -> Result<(), PeerError> {
        let (ptx, prx) = mpsc::channel::<MainThreadMessage>(32);
        let (addr, port) = loaded_peer.network_addr();
        if !self.connector.can_connect(&addr) {
            let mut db_lock = self.db.lock().await;
            db_lock.failed(&loaded_peer);
            return Err(PeerError::UnreachableSocketAddr);
        }
        crate::debug!(format!("Connecting to {:?}:{}", addr, port));
        self.current_id.increment();
        let mut peer = Peer::new(
            self.current_id,
            loaded_peer.clone(),
            self.network,
            self.block_type,
            self.mtx.clone(),
            prx,
            Arc::clone(&self.dialog),
            Arc::clone(&self.db),
            self.timeout_config,
            Arc::clone(&self.tx_queue),
        );
        let connection = self
            .connector
            .connect(addr, port, self.timeout_config.handshake_timeout)
            .await;
        let connection = match connection {
            Ok(conn) => conn,
            Err(e) => {
                let mut db_lock = self.db.lock().await;
                db_lock.failed(&loaded_peer);
                return Err(e);
            }
        };
        let is_proxy = self.connector.is_proxy();
        let handle = tokio::spawn(async move { peer.run(connection, is_proxy).await });
        self.map.insert(
            self.current_id,
            ManagedPeer {
                record: loaded_peer,
                broadcast_min: FeeRate::BROADCAST_MIN,
                ptx,
                handle,
            },
        );
        Ok(())
    }

    // Set the minimum fee rate this peer will accept
    pub fn set_broadcast_min(&mut self, nonce: PeerId, fee_rate: FeeRate) {
        if let Some(peer) = self.map.get_mut(&nonce) {
            peer.broadcast_min = fee_rate;
        }
    }

    // Set the services of a peer
    pub fn set_services(&mut self, nonce: PeerId, flags: ServiceFlags) {
        if let Some(peer) = self.map.get_mut(&nonce) {
            peer.record.update_service_flags(flags);
        }
    }

    // The minimum fee rate to successfully broadcast a transaction to all peers
    pub fn broadcast_min(&self) -> FeeRate {
        self.map
            .values()
            .map(|peer| peer.broadcast_min)
            .max()
            .unwrap_or(FeeRate::BROADCAST_MIN)
    }

    pub fn peer_info(&self) -> Vec<(AddrV2, ServiceFlags)> {
        self.map
            .values()
            .map(|peer| (peer.record.network_addr().0, peer.record.service_flags()))
            .collect()
    }

    // Send a message to the specified peer
    pub async fn send_message(&self, nonce: PeerId, message: MainThreadMessage) {
        if let Some(peer) = self.map.get(&nonce) {
            let _ = peer.ptx.send(message).await;
        }
    }

    // Broadcast to all connected peers, returning if at least one peer received the message.
    pub async fn broadcast(&self, message: MainThreadMessage) -> bool {
        let active = self.map.values().filter(|peer| !peer.handle.is_finished());
        let mut sends = Vec::new();
        for peer in active {
            let res = peer.ptx.send(message.clone()).await;
            sends.push(res.is_ok());
        }
        sends.into_iter().any(|res| res)
    }

    // Send to a random peer, returning true if the message was sent.
    pub async fn send_random(&self, message: MainThreadMessage) -> bool {
        let mut rng = StdRng::from_entropy();
        if let Some((_, peer)) = self.map.iter().choose(&mut rng) {
            let res = peer.ptx.send(message).await;
            return res.is_ok();
        }
        false
    }

    // Pull a peer from the configuration if we have one. If not, select a random peer from the database,
    // as long as it is not from the same netgroup. If there are no peers in the database, try DNS.
    // When `whitelist_only` is set, the configured peers are retried instead (see `next_whitelist_peer`).
    pub async fn next_peer(&mut self) -> Option<Record> {
        if self.whitelist_only {
            return self.next_whitelist_peer().await;
        }
        while let Some(peer) = self.whitelist.pop() {
            let port = peer
                .port
                .unwrap_or(default_port_from_network(&self.network));
            let addr = match peer.address {
                TrustedPeerInner::Addr(addr) => addr,
                TrustedPeerInner::Hostname(host) => {
                    let resolved = Self::resolve_hostname(&host, port).await;
                    if resolved.is_empty() {
                        continue;
                    }
                    crate::debug!(format!("Resolved {host} to {} address(es)", resolved.len()));
                    // Push every resolved address onto the whitelist so each is tried on
                    // a subsequent call. Reversed so the resolver's preferred order is
                    // preserved under LIFO pop.
                    for resolved_addr in resolved.into_iter().rev() {
                        self.whitelist.push(TrustedPeer {
                            address: TrustedPeerInner::Addr(resolved_addr),
                            port: Some(port),
                            known_services: peer.known_services,
                        });
                    }
                    continue;
                }
            };
            crate::debug!("Using a configured peer");
            return Some(Record::new(addr, port, peer.known_services, &LOCAL_HOST));
        }
        let mut db_lock = self.db.lock().await;
        if db_lock.is_empty() {
            crate::debug!("Bootstrapping peers with DNS");
            let new_peers = bootstrap_dns(self.network)
                .await
                .into_iter()
                .map(|ip| match ip {
                    IpAddr::V4(ip) => AddrV2::Ipv4(ip),
                    IpAddr::V6(ip) => AddrV2::Ipv6(ip),
                })
                .collect::<Vec<AddrV2>>();
            crate::debug!(format!("Adding {} sourced from DNS", new_peers.len()));
            let addr_iter = new_peers
                .into_iter()
                .map(|ip| bitcoin::p2p::address::AddrV2Message {
                    time: 0,
                    services: ServiceFlags::NONE,
                    addr: ip,
                    port: default_port_from_network(&self.network),
                });
            let source = AddrV2::Ipv4(Ipv4Addr::new(1, 1, 1, 1));
            db_lock.add_gossiped(addr_iter, &source);
        }
        db_lock.select()
    }

    // Rotate through the configured peers instead of draining them, re-resolving hostnames
    // each pass so reconnections follow DNS changes. `None` means none currently resolve.
    async fn next_whitelist_peer(&mut self) -> Option<Record> {
        for _ in 0..self.whitelist.len() {
            // Rotate so the next call tries the following peer.
            let peer = self.whitelist.remove(0);
            self.whitelist.push(peer.clone());
            let port = peer
                .port
                .unwrap_or(default_port_from_network(&self.network));
            let addr = match peer.address {
                TrustedPeerInner::Addr(addr) => addr,
                // Pick one at random so reconnections spread across a hostname's addresses.
                TrustedPeerInner::Hostname(host) => match Self::resolve_hostname(&host, port)
                    .await
                    .into_iter()
                    .choose(&mut StdRng::from_entropy())
                {
                    Some(addr) => addr,
                    None => continue,
                },
            };
            crate::debug!("Using a configured peer");
            return Some(Record::new(addr, port, peer.known_services, &LOCAL_HOST));
        }
        None
    }

    // Resolve a hostname to its current set of P2P addresses, or an empty vec on failure.
    async fn resolve_hostname(host: &str, port: u16) -> Vec<AddrV2> {
        crate::debug!(format!("Resolving hostname {host}:{port}"));
        let resolved: Vec<AddrV2> = match tokio::net::lookup_host((host, port)).await {
            Ok(iter) => iter
                .map(|sa| match sa.ip() {
                    IpAddr::V4(ip) => AddrV2::Ipv4(ip),
                    IpAddr::V6(ip) => AddrV2::Ipv6(ip),
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        if resolved.is_empty() {
            crate::debug!(format!("Could not resolve {host} to any address"));
        }
        resolved
    }

    // We tried this peer and successfully connected.
    pub async fn tried(&mut self, nonce: PeerId) {
        if let Some(peer) = self.map.get(&nonce) {
            let mut db = self.db.lock().await;
            db.tried(&peer.record);
        }
    }

    // This peer misbehaved in some way.
    pub async fn ban(&mut self, nonce: PeerId) {
        if let Some(peer) = self.map.get(&nonce) {
            let mut db = self.db.lock().await;
            db.ban(&peer.record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_map(whitelist: Whitelist, whitelist_only: bool) -> PeerMap {
        let (mtx, _mrx) = mpsc::channel(8);
        let (info_tx, _info_rx) = mpsc::channel(8);
        let (warn_tx, _warn_rx) = mpsc::unbounded_channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        PeerMap::new(
            mtx,
            Network::Regtest,
            BlockType::default(),
            whitelist,
            whitelist_only,
            Arc::new(Dialog::new(info_tx, warn_tx, event_tx)),
            ConnectionType::default(),
            PeerTimeoutConfig::default(),
        )
    }

    fn two_local_peers() -> Whitelist {
        vec![
            TrustedPeer::from_ip(Ipv4Addr::new(127, 0, 0, 1)),
            TrustedPeer::from_ip(Ipv4Addr::new(127, 0, 0, 2)),
        ]
    }

    #[tokio::test]
    async fn whitelist_only_retries_without_draining() {
        let mut peer_map = test_peer_map(two_local_peers(), true);
        // Rotated, never consumed: a peer is always available however many times we dial.
        for _ in 0..6 {
            assert!(peer_map.next_peer().await.is_some());
        }
        assert!(peer_map.will_retry_whitelist());
    }

    #[tokio::test]
    async fn gossip_mode_consumes_the_whitelist() {
        let mut peer_map = test_peer_map(two_local_peers(), false);
        assert!(peer_map.next_peer().await.is_some());
        assert!(peer_map.next_peer().await.is_some());
        // Consumed once in gossip mode; regtest has no DNS seeds or stored peers to fall back on.
        assert!(peer_map.next_peer().await.is_none());
        assert!(!peer_map.will_retry_whitelist());
    }

    #[tokio::test]
    async fn empty_whitelist_is_fatal_in_whitelist_only() {
        let mut peer_map = test_peer_map(Vec::new(), true);
        assert!(!peer_map.will_retry_whitelist());
        assert!(peer_map.next_peer().await.is_none());
    }

    #[tokio::test]
    async fn runtime_peers_join_the_retry_cycle() {
        let mut peer_map = test_peer_map(Vec::new(), true);
        assert!(!peer_map.will_retry_whitelist());
        peer_map.add_trusted_peer(TrustedPeer::from_ip(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(peer_map.will_retry_whitelist());
        // The added peer is retried like any other configured peer.
        assert!(peer_map.next_peer().await.is_some());
        assert!(peer_map.next_peer().await.is_some());
    }
}
