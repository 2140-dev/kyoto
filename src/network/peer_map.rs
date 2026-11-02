use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use addrman::Record;
use bitcoin::{
    key::rand,
    p2p::{address::AddrV2, ServiceFlags},
    FeeRate, Network, OutPoint, ScriptBuf, Transaction,
};
use rand::{rngs::StdRng, seq::IteratorRandom, SeedableRng};
use tokio::{
    sync::{
        mpsc::{self, error::TrySendError, Receiver, Sender},
        Mutex,
    },
    task::JoinHandle,
};

use crate::{
    broadcaster::BroadcastQueue,
    default_port_from_network,
    network::{
        dns::{bootstrap_dns, resolve_hostname_socks5},
        error::PeerError,
        gossip::GossipMonitor,
        peer::Peer,
        PeerId, PeerTimeoutConfig, RelayPolicy,
    },
    BlockType, Dialog, TrustedPeer, TrustedPeerInner,
};

use super::{AddressBook, ConnectionType, MainThreadMessage, PeerThreadMessage};

const LOCAL_HOST: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
// How many addresses are drawn from the address book before giving up on finding a new peer.
const NEW_PEER_SELECT_ATTEMPTS: usize = 20;

fn deliver(
    ptx: &Sender<MainThreadMessage>,
    handle: &JoinHandle<Result<(), PeerError>>,
    message: MainThreadMessage,
) -> bool {
    match ptx.try_send(message) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            handle.abort();
            false
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

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

/// A peer that is used solely for watching incoming transaction gossip.
#[derive(Debug)]
struct GossipPeer {
    id: PeerId,
    ptx: Sender<MainThreadMessage>,
    handle: JoinHandle<Result<(), PeerError>>,
}

#[derive(Debug, Default)]
enum GossipSubscriptionStatus {
    Subscribed {
        monitor: Arc<Mutex<GossipMonitor>>,
        active_peer: Option<GossipPeer>,
    },
    #[default]
    Unsubscribed,
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
    gossip: GossipSubscriptionStatus,
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
            gossip: GossipSubscriptionStatus::Unsubscribed,
            db: Arc::new(Mutex::new(AddressBook::new())),
            connector: connection_type,
            whitelist,
            dialog,
            timeout_config,
        }
    }

    // Remove any finished connections
    pub fn clean(&mut self) {
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

    pub async fn dispatch(&mut self, loaded_peer: Record) -> Result<(), PeerError> {
        let (id, peer) = self
            .spawn_peer(loaded_peer, RelayPolicy::BlocksOnly)
            .await?;
        self.map.insert(id, peer);
        Ok(())
    }

    pub async fn dispatch_gossip(&mut self, loaded_peer: Record) -> Result<(), PeerError> {
        match core::mem::take(&mut self.gossip) {
            GossipSubscriptionStatus::Subscribed {
                monitor,
                active_peer: _,
            } => {
                let relay_policy = RelayPolicy::Transactions(Arc::clone(&monitor));
                // Assign first in case the connection fails. The user still wants to be subscribed.
                self.gossip = GossipSubscriptionStatus::Subscribed {
                    monitor: Arc::clone(&monitor),
                    active_peer: None,
                };
                let (id, ManagedPeer { ptx, handle, .. }) =
                    self.spawn_peer(loaded_peer, relay_policy).await?;
                // The connection loop is live. If it fails, it will be overridden by this function
                // again.
                self.gossip = GossipSubscriptionStatus::Subscribed {
                    monitor,
                    active_peer: Some(GossipPeer { id, ptx, handle }),
                };
                Ok(())
            }
            GossipSubscriptionStatus::Unsubscribed => Ok(()),
        }
    }

    // Send out a TCP connection to a new peer and begin tracking the task
    async fn spawn_peer(
        &mut self,
        loaded_peer: Record,
        relay_policy: RelayPolicy,
    ) -> Result<(PeerId, ManagedPeer), PeerError> {
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
            relay_policy,
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
        Ok((
            self.current_id,
            ManagedPeer {
                record: loaded_peer,
                broadcast_min: FeeRate::BROADCAST_MIN,
                ptx,
                handle,
            },
        ))
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
    pub fn send_message(&self, nonce: PeerId, message: MainThreadMessage) {
        if let Some(peer) = self.map.get(&nonce) {
            deliver(&peer.ptx, &peer.handle, message);
            return;
        }
        match &self.gossip {
            GossipSubscriptionStatus::Subscribed {
                monitor: _,
                active_peer,
            } => {
                if let Some(peer) = active_peer {
                    if peer.id == nonce {
                        deliver(&peer.ptx, &peer.handle, message);
                    }
                }
            }
            GossipSubscriptionStatus::Unsubscribed => (),
        }
    }

    // Broadcast to all connected peers, returning if at least one peer received the message.
    pub fn broadcast(&self, message: MainThreadMessage) -> bool {
        let active = self.map.values().filter(|peer| !peer.handle.is_finished());
        let mut sends = Vec::new();
        for peer in active {
            sends.push(deliver(&peer.ptx, &peer.handle, message.clone()));
        }
        sends.into_iter().any(|res| res)
    }

    // Send to a random peer, returning true if the message was sent.
    pub fn send_random(&self, message: MainThreadMessage) -> bool {
        let mut rng = StdRng::from_entropy();
        if let Some((_, peer)) = self.map.iter().choose(&mut rng) {
            return deliver(&peer.ptx, &peer.handle, message);
        }
        false
    }

    // Pull a peer from the configuration if we have one. If not, select a random peer from the database,
    // as long as it is not from the same netgroup. If there are no peers in the database, try DNS.
    // When `whitelist_only` is set, only whitelist peers are used.
    pub async fn next_peer(&mut self) -> Option<Record> {
        while let Some(peer) = self.whitelist.pop() {
            let port = peer
                .port
                .unwrap_or(default_port_from_network(&self.network));
            let addr = match peer.address {
                TrustedPeerInner::Addr(addr) => addr,
                TrustedPeerInner::Hostname(host) => {
                    crate::debug!(format!("Resolving hostname {host}:{port}"));
                    let lookup = match &self.connector {
                        ConnectionType::ClearNet => tokio::net::lookup_host((host.as_str(), port))
                            .await
                            .ok()
                            .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<IpAddr>>()),
                        ConnectionType::Socks5Proxy(proxy) => {
                            resolve_hostname_socks5(proxy.0, &host)
                                .await
                                .map(|ip| vec![ip])
                        }
                    };
                    match lookup {
                        Some(ips) => {
                            let resolved: Vec<AddrV2> = ips
                                .into_iter()
                                .map(|ip| match ip {
                                    IpAddr::V4(ip) => AddrV2::Ipv4(ip),
                                    IpAddr::V6(ip) => AddrV2::Ipv6(ip),
                                })
                                .collect();
                            if resolved.is_empty() {
                                crate::debug!(format!(
                                    "Hostname {host} resolved to no addresses, skipping"
                                ));
                                continue;
                            }
                            crate::debug!(format!(
                                "Resolved {host} to {} address(es)",
                                resolved.len()
                            ));
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
                        None => {
                            crate::debug!(format!("Failed to resolve hostname {host}"));
                            continue;
                        }
                    }
                }
            };
            crate::debug!("Using a configured peer");
            return Some(Record::new(addr, port, peer.known_services, &LOCAL_HOST));
        }
        if self.whitelist_only {
            return None;
        }
        let connected: Vec<(AddrV2, u16)> = self
            .map
            .values()
            .filter(|peer| !peer.handle.is_finished())
            .map(|peer| peer.record.network_addr())
            .collect();
        let mut db_lock = self.db.lock().await;
        if db_lock.is_empty() {
            crate::debug!("Bootstrapping peers with DNS");
            let new_peers = bootstrap_dns(self.network, &self.connector)
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
        // Select a peer to which we're not yet connected
        for _ in 0..NEW_PEER_SELECT_ATTEMPTS {
            if let Some(record) = db_lock.select() {
                if !connected.contains(&record.network_addr()) {
                    return Some(record);
                }
            }
        }
        None
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

    pub async fn subscribe_to_gossip(
        &mut self,
        scripts: HashSet<ScriptBuf>,
        txins: HashSet<OutPoint>,
    ) -> Option<Receiver<Transaction>> {
        match core::mem::take(&mut self.gossip) {
            GossipSubscriptionStatus::Unsubscribed => {
                let (tx, rx) = mpsc::channel(64);
                self.gossip = GossipSubscriptionStatus::Subscribed {
                    monitor: Arc::new(Mutex::new(GossipMonitor::new(scripts, txins, tx))),
                    active_peer: None,
                };
                Some(rx)
            }
            GossipSubscriptionStatus::Subscribed {
                monitor,
                active_peer,
            } => {
                monitor.lock().await.extend(scripts, txins);
                self.gossip = GossipSubscriptionStatus::Subscribed {
                    monitor,
                    active_peer,
                };
                None
            }
        }
    }

    pub fn unsubscribe_from_gossip(&mut self) {
        match core::mem::take(&mut self.gossip) {
            GossipSubscriptionStatus::Subscribed {
                monitor: _,
                active_peer,
            } => {
                if let Some(peer) = active_peer {
                    deliver(&peer.ptx, &peer.handle, MainThreadMessage::Disconnect);
                }
                self.gossip = GossipSubscriptionStatus::Unsubscribed;
            }
            GossipSubscriptionStatus::Unsubscribed => (),
        }
    }

    // Should a new connection be established to monitor transaction gossip.
    pub fn needs_gossip_listener(&self) -> bool {
        match &self.gossip {
            GossipSubscriptionStatus::Subscribed {
                monitor: _,
                active_peer,
            } => active_peer
                .as_ref()
                .is_none_or(|peer| peer.handle.is_finished()),
            GossipSubscriptionStatus::Unsubscribed => false,
        }
    }
}
