//! This module assists in managing network addresses gossiped over the Bitcoin peer-to-peer
//! network. The goals of an address book are to prevent any single peer from filling all entries
//! in the address book, resist eclipse attacks, and to help find useful peers quickly.

use std::{
    hash::{BuildHasher, Hash, Hasher, RandomState},
    net::IpAddr,
    time::Duration,
};

use bitcoin::{
    key::rand::{seq::IteratorRandom, thread_rng},
    p2p::{address::AddrV2, ServiceFlags},
};
use tokio::time::Instant;

const ONE_MINUTE: Duration = Duration::from_secs(60);

/// A record of a potential Bitcoin peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    addr: AddrV2,
    port: u16,
    source: SourceId,
    services: ServiceFlags,
    failed_attempts: u8,
    last_attempt: Option<Instant>,
}

impl Record {
    /// Construct a new record from a gossip message.
    pub fn new(addr: AddrV2, port: u16, services: ServiceFlags, source: &IpAddr) -> Self {
        let source = source.source_id();
        Self {
            addr,
            port,
            source,
            services,
            failed_attempts: 0,
            last_attempt: None,
        }
    }

    pub fn new_from_addrv2_source(
        addr: AddrV2,
        port: u16,
        services: ServiceFlags,
        source: &AddrV2,
    ) -> Self {
        let source = match source {
            AddrV2::Ipv4(ip) => IpAddr::V4(*ip).source_id(),
            AddrV2::Ipv6(ip) => IpAddr::V6(*ip).source_id(),
            // All the mixnets go in the same source,
            _ => SourceId::Mixnet,
        };
        Self {
            addr,
            port,
            source,
            services,
            failed_attempts: 0,
            last_attempt: None,
        }
    }

    fn destination_id(&self, keys: &RandomState) -> DestinationId {
        self.addr.destination_id(keys)
    }

    fn is_same_peer(&self, other: &Record) -> bool {
        self.addr == other.addr && self.port == other.port
    }

    /// The network address and port to reach this peer.
    pub fn network_addr(&self) -> (AddrV2, u16) {
        (self.addr.clone(), self.port)
    }

    /// The services advertised by the peer.
    pub fn service_flags(&self) -> ServiceFlags {
        self.services
    }

    /// Update the most recent service flag information.
    pub fn update_service_flags(&mut self, flags: ServiceFlags) {
        self.services = flags;
    }
}

/// A table of records to store potential peers. Some properties of this table are: a single source
/// of gossip cannot fill this entire table with addresses, the table is a fixed size and held
/// entirely in memory, the table is represented as a 2D matrix.
///
/// `B` represents the number of "buckets" that hold addresses. A source may only add addresses to
/// a subset of the buckets.
///
/// `S` represents the number of "slots" per bucket. A slot is either occupied with an entry or free.
///
/// `W` is the maximum amount of buckets a source is allowed to add to, where `W < B`
///
/// A table is simply a `B x S` matrix to store peers. Limiting the buckets `B` a source may add
/// peers to creates an eclipse-resistance in the context of Bitcoin. Otherwise, this is an
/// un-ordered list.
#[derive(Debug)]
pub struct Table<const B: usize, const S: usize, const W: usize> {
    buckets: [Bucket<S>; B],
    keys: RandomState,
}

impl<const B: usize, const S: usize, const W: usize> Table<B, S, W> {
    // Used to compute a random bucket range for a `source_id`
    const RUN: usize = B / W;

    // Derive the bucket to store a record. Crucially, a single source ID cannot fill the entire
    // range of buckets.
    //
    // For example, let B = 1024, S = 64, W = 64, then RUN = 16.
    // Say the `source_id` modulo W is 63, and the `destination_id` modulo W is 7.
    //
    // We derive a bucket (63 * 16) + 7 % 1024 = 1015
    fn derive_bucket(&self, record: &Record) -> usize {
        let salt = self.keys.hash_one(record.source) as usize % W;
        let range = (salt * Self::RUN) % B;
        let index = u32::from_le_bytes(record.destination_id(&self.keys).0) as usize % W;
        (range + index) % B
    }

    fn derive_slot(&self, record: &Record) -> usize {
        self.keys.hash_one(record.destination_id(&self.keys)) as usize % S
    }

    /// Build a new table to store records of peers.
    pub fn new() -> Self {
        let buckets: [Bucket<S>; B] = [const { Bucket::new() }; B];
        Self {
            buckets,
            keys: RandomState::new(),
        }
    }

    /// Add a peer to this table. If there is a conflict at the designated slot, then the
    /// conflicting record is returned.
    ///
    /// Note that bucket and slot indices are computed deterministically, so conflicts must be
    /// resolved.
    pub fn add(&mut self, record: &Record) -> Option<Record> {
        let bucket_index = self.derive_bucket(record);
        let slot = self.derive_slot(record);
        self.buckets[bucket_index].add(slot, record.clone())
    }

    /// Remove every copy of a record from the table.
    pub fn remove(&mut self, record: &Record) {
        let slot = self.derive_slot(record);
        for bucket in &mut self.buckets {
            bucket.remove(slot, record);
        }
    }

    /// Is the entire address book empty.
    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(|bucket| bucket.is_empty())
    }

    /// Select an address uniformly at random from the address book, preferring records that
    /// have not been attempted in the last minute.
    pub fn select(&self) -> Option<Record> {
        let mut rng = thread_rng();
        let records = self
            .buckets
            .iter()
            .flat_map(|bucket| bucket.records.iter().flatten());
        records
            .clone()
            .filter(|record| {
                record
                    .last_attempt
                    .is_none_or(|last_attempt| last_attempt.elapsed() > ONE_MINUTE)
            })
            .choose(&mut rng)
            .or_else(|| records.choose(&mut rng))
            .cloned()
    }

    /// Report a successful connection to every copy of `Record`
    pub fn successful_connection(&mut self, record: &Record) {
        let slot = self.derive_slot(record);
        for bucket in &mut self.buckets {
            bucket.successful_connection(slot, record);
        }
    }

    /// Report a failed connection to every copy of `Record`, returning the most failed attempts
    /// since a successful connection among the copies if it is in the table. A failure within a
    /// minute of the previous attempt is not counted.
    pub fn failed_connection(&mut self, record: &Record) -> Option<u8> {
        let slot = self.derive_slot(record);
        self.buckets
            .iter_mut()
            .filter_map(|bucket| bucket.failed_connection(slot, record))
            .max()
    }
}

impl<const B: usize, const S: usize, const W: usize> Default for Table<B, S, W> {
    fn default() -> Self {
        Table::<B, S, W>::new()
    }
}

#[derive(Debug)]
struct Bucket<const S: usize> {
    records: [Option<Record>; S],
}

impl<const S: usize> Bucket<S> {
    const fn new() -> Self {
        let records: [Option<Record>; S] = [const { None }; S];
        Self { records }
    }

    fn add(&mut self, slot: usize, record: Record) -> Option<Record> {
        match &self.records[slot] {
            Some(occupied) => Some(occupied.clone()),
            None => {
                self.records[slot] = Some(record);
                None
            }
        }
    }

    fn matching(&mut self, slot: usize, record: &Record) -> Option<&mut Record> {
        self.records[slot]
            .as_mut()
            .filter(|occupied| occupied.is_same_peer(record))
    }

    fn remove(&mut self, slot: usize, record: &Record) {
        self.records[slot].take_if(|occupied| occupied.is_same_peer(record));
    }

    fn is_empty(&self) -> bool {
        self.records.iter().all(|record| record.is_none())
    }

    fn successful_connection(&mut self, slot: usize, record: &Record) {
        let new_flags = record.services;
        if let Some(record) = self.matching(slot, record) {
            record.last_attempt = Some(Instant::now());
            record.failed_attempts = 0;
            record.services = new_flags;
        }
    }

    fn failed_connection(&mut self, slot: usize, record: &Record) -> Option<u8> {
        let record = self.matching(slot, record)?;
        if record
            .last_attempt
            .is_none_or(|last_attempt| last_attempt.elapsed() > ONE_MINUTE)
        {
            record.failed_attempts = record.failed_attempts.saturating_add(1);
        }
        record.last_attempt = Some(Instant::now());
        Some(record.failed_attempts)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
enum SourceId {
    Ipv4([u8; 2]),
    Ipv6([u8; 4]),
    Mixnet,
}

trait SourceIdExt {
    fn source_id(&self) -> SourceId;
}

impl SourceIdExt for IpAddr {
    fn source_id(&self) -> SourceId {
        match self {
            Self::V4(ipv4) => {
                let octets = ipv4.octets();
                SourceId::Ipv4([octets[0], octets[1]])
            }
            Self::V6(ipv6) => {
                let octets = ipv6.octets();
                SourceId::Ipv6([octets[0], octets[1], octets[2], octets[3]])
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
struct DestinationId([u8; 4]);

trait DestinationIdExt {
    fn destination_id(&self, keys: &RandomState) -> DestinationId;
}

impl DestinationIdExt for AddrV2 {
    fn destination_id(&self, keys: &RandomState) -> DestinationId {
        let mut hasher = keys.build_hasher();
        match self {
            Self::Ipv4(ipv4) => {
                ipv4.octets().hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
            Self::Ipv6(ipv6) => {
                ipv6.octets().hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
            Self::I2p(i2p) => {
                i2p.hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
            Self::TorV3(tv3) => {
                tv3.hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
            Self::Cjdns(ipv6) => {
                ipv6.octets().hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
            _ => {
                "unknown network address".hash(&mut hasher);
                DestinationId(
                    hasher.finish().to_le_bytes()[..4]
                        .try_into()
                        .expect("hash is u64"),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        hash::{DefaultHasher, Hash, Hasher},
        net::{IpAddr, Ipv4Addr},
        time::SystemTime,
    };

    use bitcoin::p2p::{address::AddrV2, ServiceFlags};

    use super::{Record, Table, ONE_MINUTE};

    const LOCAL_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
    const DUMB: AddrV2 = AddrV2::Ipv4(LOCAL_HOST);

    const BUCKETS: usize = 256;
    const SLOTS: usize = 16;
    const RANGE: usize = 16;

    pub fn random_record() -> Record {
        let mut hasher = DefaultHasher::new();
        let now = SystemTime::now();
        now.hash(&mut hasher);
        let bytes = hasher.finish();
        let ip = bytes.to_le_bytes();
        let dest = Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
        let source = Ipv4Addr::new(ip[4], ip[5], ip[6], ip[7]);
        let addr_v2 = AddrV2::Ipv4(dest);
        Record::new(addr_v2, 8333, ServiceFlags::NETWORK, &IpAddr::V4(source))
    }

    #[test]
    fn test_simple_table_situations() {
        let mut table = Table::<BUCKETS, SLOTS, RANGE>::new();
        assert!(table.is_empty());
        assert!(table.select().is_none());
        let record = Record::new(DUMB, 8333, ServiceFlags::NONE, &IpAddr::V4(LOCAL_HOST));
        table.add(&record);
        assert!(!table.is_empty());
        // We should always be able to find this peer in exhaustive search.
        for _ in 0..BUCKETS * SLOTS {
            assert!(table.select().is_some());
        }
        // Adding the same record should always conflict.
        for _ in 0..BUCKETS * SLOTS {
            assert!(table.add(&record).is_some());
        }
    }

    #[test]
    fn test_every_slot_reachable() {
        let mut table = Table::<16, 16, 4>::new();
        for _ in 0..100_000 {
            table.add(&random_record());
        }
        assert!(table
            .buckets
            .iter()
            .all(|bucket| bucket.records.iter().all(Option::is_some)));
    }

    #[test]
    fn test_operations_ignore_other_peer_in_slot() {
        let mut table = Table::<1, 16, 1>::new();
        let occupant = random_record();
        table.add(&occupant);
        let other = loop {
            let candidate = random_record();
            if !candidate.is_same_peer(&occupant)
                && table.derive_slot(&candidate) == table.derive_slot(&occupant)
            {
                break candidate;
            }
        };
        table.failed_connection(&other);
        table.successful_connection(&other);
        table.remove(&other);
        assert_eq!(table.add(&occupant), Some(occupant));
    }

    #[tokio::test(start_paused = true)]
    async fn test_failures_count_once_per_minute() {
        let mut table = Table::<1, 16, 1>::new();
        let record = random_record();
        table.add(&record);
        assert_eq!(table.failed_connection(&record), Some(1));
        assert_eq!(table.failed_connection(&record), Some(1));
        tokio::time::advance(2 * ONE_MINUTE).await;
        assert_eq!(table.failed_connection(&record), Some(2));
        table.successful_connection(&record);
        assert_eq!(table.failed_connection(&record), Some(0));
    }
}
