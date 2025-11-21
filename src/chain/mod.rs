//! Structures and checkpoints related to the blockchain.
//!
//! Notably, [`checkpoints`] contains known Bitcoin block hashes and heights with significant work, so Kyoto nodes do not have to sync from genesis.
pub(crate) mod block_queue;
mod cfheader_batch;
#[allow(clippy::module_inception)]
pub(crate) mod chain;
/// Expected block header checkpoints and corresponding structure.
pub mod checkpoints;
/// Errors associated with the blockchain representation.
#[allow(dead_code)]
pub(crate) mod error;
pub(crate) mod graph;

use std::collections::HashMap;

use bitcoin::constants::SUBSIDY_HALVING_INTERVAL;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::Amount;
use bitcoin::{
    bip158::BlockFilter, block::Header, params::Params, BlockHash, FilterHash, FilterHeader,
    ScriptBuf, Target, Work,
};

use crate::network::PeerId;
use crate::HeaderCheckpoint;

use cfheader_batch::CFHeaderBatch;

type Height = u32;

/// A block header with associated height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedHeader {
    /// The height in the blockchain for this header.
    pub height: u32,
    /// The block header.
    pub header: Header,
}

impl std::cmp::PartialOrd for IndexedHeader {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for IndexedHeader {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.height.cmp(&other.height)
    }
}

impl IndexedHeader {
    pub(crate) fn new(height: u32, header: Header) -> Self {
        Self { height, header }
    }
}

impl IndexedHeader {
    /// The block hash associated with this header.
    pub fn block_hash(&self) -> BlockHash {
        self.header.block_hash()
    }

    /// The previous block hash.
    pub fn prev_blockhash(&self) -> BlockHash {
        self.header.prev_blockhash
    }
}

/// Changes applied to the chain of block headers.
#[derive(Debug, Clone)]
pub enum BlockHeaderChanges {
    /// A block was connected to the tip of the chain.
    Connected(IndexedHeader),
    /// Blocks were reorganized and a new chain of most work was selected.
    Reorganized {
        /// Newly accepted blocks from the chain of most work.
        accepted: Vec<IndexedHeader>,
        /// Blocks that were removed from the chain.
        reorganized: Vec<IndexedHeader>,
    },
    /// A peer proposed a block that is not on the chain of most work.
    ForkAdded(IndexedHeader),
}

/// A previous chain state to start the sync from.
#[derive(Debug, Clone)]
pub enum ChainState {
    /// A summary of the chain state. The vector of headers should ideally be contiguous.
    Snapshot(Vec<IndexedHeader>),
    /// A single checkpoint to start the sync _strictly after_.
    ///
    /// Note that no reorganizations can be reported.
    Checkpoint(HeaderCheckpoint),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FilterCommitment {
    pub header: FilterHeader,
    pub filter_hash: FilterHash,
}

#[derive(Debug, Clone)]
pub(crate) struct FilterRequestState {
    pub last_filter_request: Option<FilterRequest>,
    pub last_filter_header_request: Option<FilterHeaderRequest>,
    pub pending_batch: Option<(PeerId, CFHeaderBatch)>,
    pub agreement_state: FilterHeaderAgreements,
}

impl FilterRequestState {
    pub(crate) fn new(required: u8) -> Self {
        Self {
            last_filter_request: None,
            last_filter_header_request: None,
            pending_batch: None,
            agreement_state: FilterHeaderAgreements::new(required),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FilterRequest {
    #[allow(unused)]
    pub start_height: u32,
    pub stop_hash: BlockHash,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FilterHeaderRequest {
    pub start_height: u32,
    pub stop_hash: BlockHash,
    pub expected_prev_filter_header: Option<FilterHeader>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilterHeaderAgreements {
    current: u8,
    required: u8,
}

impl FilterHeaderAgreements {
    pub(crate) fn new(required: u8) -> Self {
        Self {
            current: 0,
            required,
        }
    }

    pub(crate) fn got_agreement(&mut self) {
        self.current += 1;
    }

    pub(crate) fn enough_agree(&self) -> bool {
        self.current.ge(&self.required)
    }

    pub(crate) fn reset_agreements(&mut self) {
        self.current = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CFHeaderChanges {
    AddedToQueue,
    Extended,
    // Unfortunately, auditing each peer by reconstruction the filter would be costly in network
    // and compute. Instead it is easier to disconnect from all peers and try again.
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FilterCheck {
    // This filter was for the `stop_hash`
    pub(crate) was_last_in_batch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filter {
    filter_hash: FilterHash,
    block_hash: BlockHash,
    block_filter: BlockFilter,
}

#[allow(dead_code)]
impl Filter {
    pub fn new(contents: Vec<u8>, block_hash: BlockHash) -> Self {
        let hash = sha256d::Hash::hash(&contents);
        let filter_hash = FilterHash::from_raw_hash(hash);
        let block_filter = BlockFilter::new(&contents);
        Self {
            filter_hash,
            block_hash,
            block_filter,
        }
    }

    pub fn filter_hash(&self) -> &FilterHash {
        &self.filter_hash
    }

    pub fn block_hash(&self) -> BlockHash {
        self.block_hash
    }

    pub fn contains_any<'a>(&'a self, scripts: impl Iterator<Item = &'a ScriptBuf>) -> bool {
        self.block_filter
            .match_any(&self.block_hash, scripts.map(|script| script.to_bytes()))
            .expect("vec reader is infallible")
    }

    pub fn into_filter(self) -> BlockFilter {
        self.block_filter
    }

    pub fn contents(self) -> Vec<u8> {
        self.block_filter.content
    }
}

#[derive(Debug)]
pub(crate) struct HeightMonitor {
    map: HashMap<PeerId, Height>,
}

impl HeightMonitor {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub(crate) fn max(&self) -> Option<Height> {
        self.map.values().copied().max()
    }

    pub(crate) fn retain(&mut self, peers: &[PeerId]) {
        self.map.retain(|peer_id, _| peers.contains(peer_id));
    }

    pub(crate) fn insert(&mut self, peer_id: PeerId, height: Height) {
        self.map.insert(peer_id, height);
    }

    pub(crate) fn increment(&mut self, peer_id: PeerId) {
        if let Some(height) = self.map.get_mut(&peer_id) {
            *height = height.saturating_add(1);
        }
    }
}

// Attributes of a height in the Bitcoin blockchain.
trait HeightExt: Clone + Copy + std::hash::Hash + PartialEq + Eq + PartialOrd + Ord {
    fn increment(&self) -> Self;

    fn from_u64_checked(height: u64) -> Option<Self>;

    fn is_adjustment_multiple(&self, params: impl AsRef<Params>) -> bool;
}

impl HeightExt for u32 {
    fn increment(&self) -> Self {
        self + 1
    }

    fn is_adjustment_multiple(&self, params: impl AsRef<Params>) -> bool {
        *self as u64 % params.as_ref().difficulty_adjustment_interval() == 0
    }

    fn from_u64_checked(height: u64) -> Option<Self> {
        height.try_into().ok()
    }
}

trait ZerolikeExt {
    fn zero() -> Self;
}

impl ZerolikeExt for Work {
    fn zero() -> Self {
        Self::from_be_bytes([0; 32])
    }
}

trait HeaderValidationExt {
    // Headers are logically connected.
    fn connected(&self) -> bool;
    // Each header passes its own work target.
    fn passes_own_pow(&self) -> bool;
    // Targets do not change out of the acceptable range.
    fn bits_adhere_transition_threshold(&self, params: impl AsRef<Params>) -> bool;
}

impl HeaderValidationExt for &[Header] {
    fn connected(&self) -> bool {
        self.iter()
            .zip(self.iter().skip(1))
            .all(|(first, second)| first.block_hash().eq(&second.prev_blockhash))
    }

    fn passes_own_pow(&self) -> bool {
        !self.iter().any(|header| {
            let target = header.target();
            let valid_pow = header.validate_pow(target);
            valid_pow.is_err()
        })
    }

    fn bits_adhere_transition_threshold(&self, params: impl AsRef<Params>) -> bool {
        let params = params.as_ref();
        if params.allow_min_difficulty_blocks {
            return true;
        }
        self.iter().zip(self.iter().skip(1)).all(|(first, second)| {
            let transition = Target::from_compact(first.bits).max_transition_threshold(params);
            Target::from_compact(second.bits).le(&transition)
        })
    }
}

// Emulation of `GetBlockSubsidy` in Bitcoin Core: https://github.com/bitcoin/bitcoin/blob/master/src/validation.cpp#L1944
pub(crate) fn block_subsidy(height: u32) -> Amount {
    let halvings = height / SUBSIDY_HALVING_INTERVAL;
    if halvings >= 64 {
        return Amount::ZERO;
    }
    let base = Amount::ONE_BTC.to_sat() * 50;
    let subsidy = base >> halvings;
    Amount::from_sat(subsidy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_height_monitor() {
        let peer_one = 1.into();
        let peer_two = 2.into();
        let peer_three = 3.into();

        let mut height_monitor = HeightMonitor::new();
        height_monitor.insert(peer_one, 10);
        assert!(height_monitor.max().unwrap().eq(&10));
        height_monitor.insert(peer_two, 11);
        height_monitor.insert(peer_three, 12);
        assert!(height_monitor.max().unwrap().eq(&12));
        // this should remove peer three
        height_monitor.retain(&[peer_one, peer_two]);
        assert!(height_monitor.max().unwrap().eq(&11));
        height_monitor.retain(&[]);
        assert!(height_monitor.max().is_none());
        height_monitor.insert(peer_one, 10);
        assert!(height_monitor.max().unwrap().eq(&10));
        height_monitor.insert(peer_two, 11);
        // peer one is now at 11
        height_monitor.increment(peer_one);
        // peer one is now at 12
        height_monitor.increment(peer_one);
        assert!(height_monitor.max().unwrap().eq(&12));
    }

    #[test]
    fn test_subsidy_calculation() {
        let first_subsidy = block_subsidy(2);
        assert_eq!(first_subsidy, Amount::from_btc(50.0).unwrap());
        let first_subsidy = block_subsidy(209_999);
        assert_eq!(first_subsidy, Amount::from_btc(50.0).unwrap());
        let second_subsidy = block_subsidy(210_000);
        assert_eq!(second_subsidy, Amount::from_btc(25.0).unwrap());
        let fourth_subsidy = block_subsidy(630_000);
        assert_eq!(fourth_subsidy, Amount::from_btc(6.25).unwrap());
        let now = block_subsidy(902_000);
        assert_eq!(now, Amount::from_btc(3.125).unwrap());
    }
}
