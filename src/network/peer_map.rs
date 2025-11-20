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
    chain::HeightMonitor,
    default_port_from_network,
    network::{dns::bootstrap_dns, error::PeerError, peer::Peer, PeerId, PeerTimeoutConfig},
    Dialog, TrustedPeer,
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
    current_id: PeerId,
    heights: Arc<Mutex<HeightMonitor>>,
    network: Network,
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
        whitelist: Whitelist,
        dialog: Arc<Dialog>,
        connection_type: ConnectionType,
        timeout_config: PeerTimeoutConfig,
        height_monitor: Arc<Mutex<HeightMonitor>>,
    ) -> Self {
        Self {
            tx_queue: Arc::new(Mutex::new(BroadcastQueue::new())),
            current_id: PeerId(0),
            heights: height_monitor,
            network,
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
        let active = self.map.keys().copied().collect::<Vec<PeerId>>();
        let mut height_lock = self.heights.lock().await;
        height_lock.retain(&active);
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
        let handle = tokio::spawn(async move { peer.run(connection).await });
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

    // Set the height of a peer upon receiving the version message
    pub async fn set_height(&mut self, nonce: PeerId, height: u32) {
        let mut height_lock = self.heights.lock().await;
        height_lock.insert(nonce, height);
    }

    // Add one to the height of a peer when receiving inventory
    pub async fn increment_height(&mut self, nonce: PeerId) {
        let mut height_lock = self.heights.lock().await;
        height_lock.increment(nonce);
    }

    // The minimum fee rate to successfully broadcast a transaction to all peers
    pub fn broadcast_min(&self) -> FeeRate {
        self.map
            .values()
            .map(|peer| peer.broadcast_min)
            .max()
            .unwrap_or(FeeRate::BROADCAST_MIN)
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
    pub async fn next_peer(&mut self) -> Option<Record> {
        if let Some(peer) = self.whitelist.pop() {
            crate::debug!("Using a configured peer");
            let port = peer
                .port
                .unwrap_or(default_port_from_network(&self.network));
            let record = Record::new(peer.address(), port, peer.known_services, &LOCAL_HOST);
            return Some(record);
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
