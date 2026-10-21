use std::collections::{BTreeMap, HashMap};

use crate::HashCheckpoint;

use bitcoin::{
    block::Header, constants::genesis_block, BlockHash, CompactTarget, FilterHash, Network, Work,
};

use super::{FilterCommitment, HeightExt, IndexedHeader, ZerolikeExt};

type Height = u32;

const LOCATOR_INDEX: &[Height] = &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];

#[derive(Debug, Clone)]
pub(crate) enum AcceptHeaderChanges {
    Accepted {
        connected_at: IndexedHeader,
    },
    Duplicate,
    ExtendedFork {
        connected_at: IndexedHeader,
    },
    Reorganization {
        accepted: Vec<IndexedHeader>,
        disconnected: Vec<IndexedHeader>,
    },
    Rejected(HeaderRejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderRejection {
    InvalidPow {
        expected: CompactTarget,
        got: CompactTarget,
    },
    UnknownPrevHash(BlockHash),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Tip {
    pub hash: BlockHash,
    pub height: Height,
    pub next_work_required: Option<CompactTarget>,
}

impl Tip {
    pub(crate) fn from_checkpoint(height: Height, hash: BlockHash) -> Self {
        Self {
            hash,
            height,
            next_work_required: None,
        }
    }
}

impl From<HashCheckpoint> for Tip {
    fn from(value: HashCheckpoint) -> Self {
        Tip::from_checkpoint(value.height, value.hash)
    }
}

impl From<IndexedHeader> for Tip {
    fn from(value: IndexedHeader) -> Self {
        let hash = value.block_hash();
        Tip::from_checkpoint(value.height, hash)
    }
}

#[derive(Debug, Clone, Hash)]
pub(crate) struct BlockNode {
    pub height: Height,
    pub header: Header,
    pub acc_work: Work,
    pub filter_commitment: Option<FilterCommitment>,
    pub filter_checked: bool,
    pub cf_header_assumed: bool,
}

impl BlockNode {
    fn new(height: Height, header: Header, acc_work: Work) -> Self {
        Self {
            height,
            header,
            acc_work,
            filter_commitment: None,
            filter_checked: false,
            cf_header_assumed: false,
        }
    }
}

#[derive(Debug)]
pub struct BlockTree {
    canonical_hashes: BTreeMap<Height, BlockHash>,
    headers: HashMap<BlockHash, BlockNode>,
    active_tip: Tip,
    candidate_forks: Vec<Tip>,
    network: Network,
    total_filters_synced: u32,
    total_filter_headers_synced: u32,
}

#[allow(unused)]
impl BlockTree {
    pub(crate) fn new(tip: impl Into<Tip>, network: Network) -> Self {
        let tip = tip.into();
        Self {
            canonical_hashes: BTreeMap::new(),
            headers: HashMap::with_capacity(20_000),
            active_tip: tip,
            candidate_forks: Vec::with_capacity(2),
            network,
            total_filters_synced: 0,
            total_filter_headers_synced: 0,
        }
    }

    pub(crate) fn from_genesis(network: Network) -> Self {
        let genesis = genesis_block(network);
        let height = 0;
        let hash = genesis.block_hash();
        let tip = Tip {
            hash,
            height,
            next_work_required: Some(genesis.header.bits),
        };
        let headers = HashMap::with_capacity(20_000);
        Self {
            canonical_hashes: BTreeMap::new(),
            headers,
            active_tip: tip,
            candidate_forks: Vec::with_capacity(2),
            network,
            total_filters_synced: 0,
            total_filter_headers_synced: 0,
        }
    }

    pub(crate) fn accept_header(&mut self, new_header: Header) -> AcceptHeaderChanges {
        let new_hash = new_header.block_hash();
        let prev_hash = new_header.prev_blockhash;

        if self.active_tip.hash.eq(&prev_hash) {
            let new_height = self.active_tip.height.increment();
            let params = self.network.params();
            let next_work = if !params.no_pow_retargeting
                && !params.allow_min_difficulty_blocks
                && new_height.is_adjustment_multiple(self.network)
            {
                self.compute_next_work_required(new_height)
            } else {
                self.active_tip.next_work_required
            };
            if let Some(work) = next_work {
                if new_header.bits.ne(&work) {
                    return AcceptHeaderChanges::Rejected(HeaderRejection::InvalidPow {
                        expected: work,
                        got: new_header.bits,
                    });
                }
            }
            let new_tip = Tip {
                hash: new_hash,
                height: new_height,
                next_work_required: next_work,
            };
            let prev_work = self
                .headers
                .get(&prev_hash)
                .map(|block| block.acc_work)
                .unwrap_or(Work::zero());
            let new_work = prev_work + new_header.work();
            let new_block_node = BlockNode::new(new_height, new_header, new_work);
            self.headers.insert(new_hash, new_block_node);
            self.active_tip = new_tip;
            self.canonical_hashes.insert(new_height, new_hash);
            return AcceptHeaderChanges::Accepted {
                connected_at: IndexedHeader::new(new_height, new_header),
            };
        }

        if self.headers.contains_key(&new_hash) {
            return AcceptHeaderChanges::Duplicate;
        }

        if let Some(fork_index) = self
            .candidate_forks
            .iter()
            .position(|fork| fork.hash.eq(&prev_hash))
        {
            let fork = self.candidate_forks[fork_index];
            if let Some(node) = self.headers.get(&fork.hash) {
                let new_height = node.height.increment();
                let params = self.network.params();
                let next_work = if !params.no_pow_retargeting
                    && !params.allow_min_difficulty_blocks
                    && new_height.is_adjustment_multiple(self.network)
                {
                    self.compute_next_work_required(new_height)
                } else {
                    fork.next_work_required
                };
                if let Some(work) = next_work {
                    if new_header.bits.ne(&work) {
                        return AcceptHeaderChanges::Rejected(HeaderRejection::InvalidPow {
                            expected: work,
                            got: new_header.bits,
                        });
                    }
                }
                self.candidate_forks.swap_remove(fork_index);
                let acc_work = node.acc_work + new_header.work();
                let new_tip = Tip {
                    hash: new_hash,
                    height: new_height,
                    next_work_required: next_work,
                };
                let new_block_node = BlockNode::new(new_height, new_header, acc_work);
                self.headers.insert(new_hash, new_block_node);
                if acc_work
                    > self
                        .headers
                        .get(&self.active_tip.hash)
                        .map(|node| node.acc_work)
                        .unwrap_or(Work::zero())
                {
                    self.candidate_forks.push(self.active_tip);
                    self.active_tip = new_tip;
                    let (accepted, disconnected) = self.switch_to_fork(&new_tip);
                    return AcceptHeaderChanges::Reorganization {
                        accepted,
                        disconnected,
                    };
                } else {
                    self.candidate_forks.push(new_tip);
                    return AcceptHeaderChanges::ExtendedFork {
                        connected_at: IndexedHeader::new(new_height, new_header),
                    };
                }
            }
        }

        match self.headers.get(&prev_hash) {
            // A new fork was detected
            Some(node) => {
                let new_height = node.height.increment();
                let params = self.network.params();
                let next_work = if !params.no_pow_retargeting
                    && !params.allow_min_difficulty_blocks
                    && new_height.is_adjustment_multiple(self.network)
                {
                    self.compute_next_work_required(new_height)
                } else {
                    Some(node.header.bits)
                };
                if let Some(work) = next_work {
                    if new_header.bits.ne(&work) {
                        return AcceptHeaderChanges::Rejected(HeaderRejection::InvalidPow {
                            expected: work,
                            got: new_header.bits,
                        });
                    }
                }
                let acc_work = node.acc_work + new_header.work();
                let new_tip = Tip {
                    hash: new_hash,
                    height: new_height,
                    next_work_required: next_work,
                };
                self.candidate_forks.push(new_tip);
                let new_block_node = BlockNode::new(new_height, new_header, acc_work);
                self.headers.insert(new_hash, new_block_node);
                AcceptHeaderChanges::ExtendedFork {
                    connected_at: IndexedHeader::new(new_height, new_header),
                }
            }
            // This chain doesn't link to ours in any known way
            None => AcceptHeaderChanges::Rejected(HeaderRejection::UnknownPrevHash(prev_hash)),
        }
    }

    fn switch_to_fork(&mut self, new_best: &Tip) -> (Vec<IndexedHeader>, Vec<IndexedHeader>) {
        let mut curr_hash = new_best.hash;
        let mut connections = Vec::new();
        let mut disconnections = Vec::new();
        while let Some((&height, &hash)) = self.canonical_hashes.last_key_value() {
            if height <= new_best.height {
                break;
            }
            self.canonical_hashes.remove(&height);
            if let Some(node) = self.headers.get(&hash) {
                disconnections.push(IndexedHeader::new(node.height, node.header));
                if node.filter_checked {
                    self.total_filters_synced -= 1;
                }
                if node.filter_commitment.is_some() || node.cf_header_assumed {
                    self.total_filter_headers_synced -= 1;
                }
            }
        }
        loop {
            match self.headers.get(&curr_hash) {
                Some(node) => {
                    let next = node.header.prev_blockhash;
                    match self.canonical_hashes.get_mut(&node.height) {
                        Some(canonical_hash) => {
                            let reorged_hash = *canonical_hash;
                            if reorged_hash.ne(&curr_hash) {
                                if let Some(reorged) = self.headers.get(&reorged_hash) {
                                    disconnections
                                        .push(IndexedHeader::new(reorged.height, reorged.header));
                                    if reorged.filter_checked {
                                        self.total_filters_synced -= 1;
                                    }
                                    if reorged.filter_commitment.is_some()
                                        || reorged.cf_header_assumed
                                    {
                                        self.total_filter_headers_synced -= 1;
                                    }
                                }
                                *canonical_hash = curr_hash;
                                connections.push(IndexedHeader::new(node.height, node.header));
                                if node.filter_checked {
                                    self.total_filters_synced += 1;
                                }
                                if node.filter_commitment.is_some() || node.cf_header_assumed {
                                    self.total_filter_headers_synced += 1;
                                }

                                curr_hash = next;
                            } else {
                                return (connections, disconnections);
                            }
                        }
                        None => {
                            self.canonical_hashes.insert(node.height, curr_hash);
                            connections.push(IndexedHeader::new(node.height, node.header));
                            if node.filter_checked {
                                self.total_filters_synced += 1;
                            }
                            if node.filter_commitment.is_some() || node.cf_header_assumed {
                                self.total_filter_headers_synced += 1;
                            }

                            curr_hash = next;
                        }
                    }
                }
                None => return (connections, disconnections),
            }
        }
    }

    fn compute_next_work_required(&self, new_height: Height) -> Option<CompactTarget> {
        // Do not audit the diffulty for `Testnet`. Auditing the difficulty properly for a testnet
        // will result in convoluted logic. This is a critical code block for mainnet and should be
        // as readable as possible
        if self.network.params().allow_min_difficulty_blocks {
            return None;
        }
        let adjustment_period =
            Height::from_u64_checked(self.network.params().difficulty_adjustment_interval())?;
        let epoch_start = new_height.checked_sub(adjustment_period)?;
        let epoch_end = new_height.checked_sub(1)?;
        let epoch_start_hash = self.canonical_hashes.get(&epoch_start)?;
        let epoch_end_hash = self.canonical_hashes.get(&epoch_end)?;
        let epoch_start_header = self.headers.get(epoch_start_hash).map(|node| node.header)?;
        let epoch_end_header = self.headers.get(epoch_end_hash).map(|node| node.header)?;
        let new_target = CompactTarget::from_header_difficulty_adjustment(
            epoch_start_header,
            epoch_end_header,
            self.network,
        );
        Some(new_target)
    }

    pub(crate) fn block_hash_at_height(&self, height: Height) -> Option<BlockHash> {
        if self.active_tip.height.eq(&height) {
            return Some(self.active_tip.hash);
        }
        self.canonical_hashes.get(&height).copied()
    }

    pub(crate) fn header_at_height(&self, height: Height) -> Option<Header> {
        let hash = self.canonical_hashes.get(&height)?;
        self.headers.get(hash).map(|node| node.header)
    }

    // Returns the height of `hash` only when it sits on the canonical chain of most work.
    // Returns `None` for unknown hashes and for hashes on stale/reorganized branches.
    pub(crate) fn height_of_hash_canonical_only(&self, hash: BlockHash) -> Option<Height> {
        let height = self.headers.get(&hash).map(|node| node.height)?;
        let canonical = self.block_hash_at_height(height)?;
        (canonical == hash).then_some(height)
    }

    pub(crate) fn height_of_hash(&self, hash: BlockHash) -> Option<Height> {
        self.headers.get(&hash).map(|node| node.height)
    }

    pub(crate) fn header_at_hash(&self, hash: BlockHash) -> Option<Header> {
        self.headers.get(&hash).map(|node| node.header)
    }

    pub(crate) fn height(&self) -> Height {
        self.active_tip.height
    }

    pub(crate) fn contains(&self, hash: BlockHash) -> bool {
        self.headers.contains_key(&hash) || self.active_tip.hash.eq(&hash)
    }

    pub(crate) fn tip_hash(&self) -> BlockHash {
        self.active_tip.hash
    }

    pub(crate) fn filter_hash(&self, block_hash: BlockHash) -> Option<FilterHash> {
        Some(
            self.headers
                .get(&block_hash)?
                .filter_commitment?
                .filter_hash,
        )
    }

    pub(crate) fn filter_commitment(&self, block_hash: BlockHash) -> Option<&FilterCommitment> {
        self.headers.get(&block_hash)?.filter_commitment.as_ref()
    }

    pub(crate) fn filter_hash_at_height(&self, height: impl Into<Height>) -> Option<FilterHash> {
        let height = height.into();
        let hash = self.canonical_hashes.get(&height)?;
        Some(self.headers.get(hash)?.filter_commitment?.filter_hash)
    }

    pub(crate) fn set_commitment(&mut self, commitment: FilterCommitment, hash: BlockHash) {
        if let Some(node) = self.headers.get_mut(&hash) {
            //As it is a pub function, a caller could set_commitment twice for the same block, we
            //don't want to increment counters in that case. But in normal operation it is not
            //possible (perhaps).
            debug_assert!(node.filter_commitment.is_none() && !node.cf_header_assumed);
            node.filter_commitment = Some(commitment);
            if self.canonical_hashes.get(&node.height) == Some(&hash) {
                self.total_filter_headers_synced += 1;
            }
        }
    }

    pub(crate) fn check_filter(&mut self, hash: BlockHash) {
        if let Some(node) = self.headers.get_mut(&hash) {
            debug_assert!(!node.filter_checked);
            node.filter_checked = true;
            //As it is a pub function, a caller could check_filter twice for the same block, we
            //don't want to increment counters in that case. But in normal operation it is not
            //possible (perhaps).
            if self.canonical_hashes.get(&node.height) == Some(&hash) {
                self.total_filters_synced += 1;
            }
        }
    }

    pub(crate) fn assume_checked_to(&mut self, assumed_height: Height) {
        let mut curr = self.tip_hash();
        while let Some(node) = self.headers.get_mut(&curr) {
            if node.height <= assumed_height {
                if !(node.filter_commitment.is_some() || node.cf_header_assumed) {
                    self.total_filter_headers_synced += 1;
                }
                if !node.filter_checked {
                    self.total_filters_synced += 1;
                }
                node.filter_checked = true;
                node.cf_header_assumed = true;
            }
            curr = node.header.prev_blockhash
        }
    }

    pub(crate) fn is_filter_checked(&self, hash: &BlockHash) -> bool {
        if let Some(node) = self.headers.get(hash) {
            return node.filter_checked;
        }
        false
    }

    pub(crate) fn reset_filter_state(&mut self) {
        for node in self.headers.values_mut() {
            node.filter_checked = false;
            node.cf_header_assumed = false;
            node.filter_commitment = None;
        }
        self.total_filters_synced = 0;
        self.total_filter_headers_synced = 0;
    }

    pub(crate) fn filter_headers_synced(&self) -> bool {
        let synced = self.total_filter_headers_synced as usize == self.internal_chain_len();
        // The sync state depends on this, so check the counter against a walk in debug builds.
        debug_assert_eq!(
            synced,
            self.iter_data()
                .all(|node| node.filter_commitment.is_some() || node.cf_header_assumed)
        );
        synced
    }

    pub(crate) fn filters_synced(&self) -> bool {
        let synced = self.total_filters_synced as usize == self.internal_chain_len();
        // The sync state depends on this, so check the counter against a walk in debug builds.
        debug_assert_eq!(synced, self.iter_data().all(|node| node.filter_checked));
        synced
    }

    pub(crate) fn total_filters_synced(&self) -> u32 {
        self.total_filters_synced
    }

    pub(crate) fn total_filter_headers_synced(&self) -> u32 {
        self.total_filter_headers_synced
    }

    // The same counts as the counters, computed by walking the whole canonical chain. This is how
    // they were computed before the counters existed, and it is used in debug builds to check them.
    #[cfg(debug_assertions)]
    pub(crate) fn filter_counts_by_walk(&self) -> (u32, u32) {
        let filter_headers = self
            .iter_data()
            .filter(|node| node.filter_commitment.is_some() || node.cf_header_assumed)
            .count() as u32;
        let filters = self.iter_data().filter(|node| node.filter_checked).count() as u32;
        (filter_headers, filters)
    }

    pub(crate) fn locators(&self) -> Vec<BlockHash> {
        let mut locators = Vec::new();
        locators.push(self.active_tip.hash);
        for locator in LOCATOR_INDEX {
            let height = self.active_tip.height.checked_sub(*locator);
            match height {
                Some(height) => match self.block_hash_at_height(height) {
                    Some(hash) => locators.push(hash),
                    None => return locators.into_iter().rev().collect(),
                },
                None => return locators.into_iter().rev().collect(),
            }
        }
        locators.into_iter().rev().collect()
    }

    pub(crate) fn internal_chain_len(&self) -> usize {
        self.canonical_hashes.len()
    }

    pub(crate) fn iter_data(&self) -> BlockNodeIterator<'_> {
        BlockNodeIterator {
            block_tree: self,
            current: self.active_tip.hash,
        }
    }

    pub(crate) fn iter_headers(&self) -> BlockHeaderIterator<'_> {
        BlockHeaderIterator {
            block_tree: self,
            current: self.active_tip.hash,
        }
    }
}

pub(crate) struct BlockHeaderIterator<'a> {
    block_tree: &'a BlockTree,
    current: BlockHash,
}

impl Iterator for BlockHeaderIterator<'_> {
    type Item = IndexedHeader;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.block_tree.headers.get(&self.current)?;
        self.current = node.header.prev_blockhash;
        Some(IndexedHeader::new(node.height, node.header))
    }
}

pub(crate) struct BlockNodeIterator<'a> {
    block_tree: &'a BlockTree,
    current: BlockHash,
}

impl<'a> Iterator for BlockNodeIterator<'a> {
    type Item = &'a BlockNode;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.block_tree.headers.get(&self.current)?;
        self.current = node.header.prev_blockhash;
        Some(node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corepc_node::serde_json;
    use std::fs::File;
    use std::str::FromStr;

    #[derive(Debug, Clone)]
    struct HexHeader(Header);
    crate::impl_deserialize!(HexHeader, Header);

    #[derive(Debug, Clone, serde::Deserialize)]
    struct GraphScenario {
        // Blocks that do not get reorged.
        base: Vec<HexHeader>,
        // Blocks that get reorged or become old
        stale: Vec<HexHeader>,
        // New competiting blocks
        new: Vec<HexHeader>,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct GraphTestFile {
        scenarios: Vec<GraphScenario>,
    }

    fn get_graph_scenario(index: usize) -> GraphScenario {
        let file = File::open("./tests/data/graph_scenarios.json").unwrap();
        let graph_cases: GraphTestFile = serde_json::from_reader(&file).unwrap();
        graph_cases.scenarios[index].clone()
    }

    #[test]
    fn test_depth_one_reorg() {
        let GraphScenario { base, stale, new } = get_graph_scenario(0);
        let tip = Tip::from_checkpoint(
            7,
            BlockHash::from_str("62c28f380692524a3a8f1fc66252bc0eb31d6b6a127d2263bdcbee172529fe16")
                .unwrap(),
        );
        let mut chain = BlockTree::new(tip, Network::Regtest);
        for header in &base {
            let accept = chain.accept_header(header.0);
            assert!(matches!(
                accept,
                AcceptHeaderChanges::Accepted { connected_at: _ }
            ));
        }
        for header in &stale {
            let accept = chain.accept_header(header.0);
            assert!(matches!(
                accept,
                AcceptHeaderChanges::Accepted { connected_at: _ }
            ));
        }
        // We just added 3 headers starting at height 7, so this should check out.
        assert_eq!(chain.height(), 10);
        let mut new_header_iter = new.into_iter().map(|hex| hex.0);
        let new_block_10 = new_header_iter.next().unwrap();
        let accept_new_10 = chain.accept_header(new_block_10);
        assert!(matches!(
            accept_new_10,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 10);
        let old_block_10 = stale.first().unwrap().0;
        assert_eq!(chain.header_at_height(10), Some(old_block_10));
        let block_11 = new_header_iter.next().unwrap();
        let accept_11 = chain.accept_header(block_11);
        match accept_11 {
            AcceptHeaderChanges::Reorganization {
                accepted,
                disconnected,
            } => {
                assert_eq!(
                    accepted,
                    vec![
                        IndexedHeader::new(11, block_11),
                        IndexedHeader::new(10, new_block_10)
                    ]
                );
                assert_eq!(old_block_10, disconnected.first().unwrap().header);
                assert_eq!(10, disconnected.first().unwrap().height);
                assert_eq!(1, disconnected.len())
            }
            _ => panic!("reorganization should have been accepted"),
        }
        assert_eq!(chain.header_at_height(12), None);
        assert_eq!(chain.header_at_height(11), Some(block_11));
        assert_eq!(chain.header_at_height(10), Some(new_block_10));
        assert_eq!(chain.header_at_height(9), Some(base[1].0));
        assert_eq!(chain.header_at_height(8), Some(base[0].0));
        // height_of_hash returns the height for hashes on the canonical chain
        assert_eq!(chain.height_of_hash(new_block_10.block_hash()), Some(10));
        assert_eq!(chain.height_of_hash(block_11.block_hash()), Some(11));
        // ...and None for hashes that were reorganized away, even though the header
        // is still stored in the graph.
        assert_eq!(
            chain.height_of_hash_canonical_only(old_block_10.block_hash()),
            None
        );
        // Unknown hashes also return None.
        let unknown =
            BlockHash::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap();
        assert_eq!(chain.height_of_hash(unknown), None);
    }

    #[test]
    fn test_depth_two_reorg() {
        let GraphScenario { base, stale, new } = get_graph_scenario(1);
        let mut chain = BlockTree::from_genesis(Network::Regtest);
        for header in &base {
            let accept = chain.accept_header(header.0);
            assert!(matches!(
                accept,
                AcceptHeaderChanges::Accepted { connected_at: _ }
            ));
        }
        for header in &stale {
            let accept = chain.accept_header(header.0);
            assert!(matches!(
                accept,
                AcceptHeaderChanges::Accepted { connected_at: _ }
            ));
        }
        // We started from genesis and add 4 blocks.
        assert_eq!(chain.height(), 4);
        let mut new_header_iter = new.into_iter().map(|hex| hex.0);
        let new_block_3 = new_header_iter.next().unwrap();
        // Create a new fork
        let accept_new_3 = chain.accept_header(new_block_3);
        assert!(matches!(
            accept_new_3,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 4);
        assert_eq!(chain.header_at_height(3), Some(stale[0].0));
        // Advertise the same fork
        let accept_new_3 = chain.accept_header(new_block_3);
        assert!(matches!(accept_new_3, AcceptHeaderChanges::Duplicate));
        assert_eq!(chain.height(), 4);
        // Extend the fork, but do not switch to it
        let new_block_4 = new_header_iter.next().unwrap();
        let accept_new_4 = chain.accept_header(new_block_4);
        assert!(matches!(
            accept_new_4,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 4);
        assert_eq!(chain.header_at_height(4), Some(stale[1].0));
        assert_eq!(chain.header_at_height(3), Some(stale[0].0));
        assert_eq!(chain.header_at_height(2), Some(base[1].0));
        assert_eq!(chain.header_at_height(1), Some(base[0].0));
    }

    #[test]
    fn test_assumed_checked() {
        let GraphScenario {
            base,
            stale: _,
            new: _,
        } = get_graph_scenario(3);
        let mut chain = BlockTree::from_genesis(Network::Regtest);
        for header in base.into_iter().map(|hex| hex.0) {
            chain.accept_header(header);
        }
        chain.assume_checked_to(3);
        assert!(!chain.filters_synced());
        assert!(!chain.filter_headers_synced());
        chain.assume_checked_to(4);
        assert!(chain.filters_synced());
        assert!(chain.filter_headers_synced());
    }

    fn store_node(
        tree: &mut BlockTree,
        height: u32,
        header: Header,
        previous_work: Work,
        canonical: bool,
    ) -> Work {
        let work = previous_work + header.work();
        tree.headers
            .insert(header.block_hash(), BlockNode::new(height, header, work));
        if canonical {
            tree.canonical_hashes.insert(height, header.block_hash());
        }
        work
    }

    #[test]
    fn test_shorter_fork_reorg() {
        let network = Network::Bitcoin;
        let hard = CompactTarget::from_consensus(0x1c0ffff0);
        let easy = CompactTarget::from_consensus(0x1c3fffc0);
        let mut common = genesis_block(network).header;
        common.bits = hard;
        common.time = 1_600_000_000 + 1_000_000;
        let mut tree = BlockTree::new(Tip::from_checkpoint(2013, common.prev_blockhash), network);
        let common_work = store_node(&mut tree, 2014, common, Work::zero(), true);
        let mut old = common;
        old.prev_blockhash = common.block_hash();
        old.time = 1_600_000_000 + 4 * 1_209_600;
        let mut old_work = store_node(&mut tree, 2015, old, common_work, true);
        let mut old_branch = vec![old];
        for height in 2016..=2020 {
            let mut next = old;
            next.prev_blockhash = old.block_hash();
            next.time += 600;
            next.bits = easy;
            old_work = store_node(&mut tree, height, next, old_work, true);
            old_branch.push(next);
            old = next;
        }
        tree.active_tip = Tip {
            hash: old.block_hash(),
            height: 2020,
            next_work_required: Some(easy),
        };

        let mut fork = common;
        fork.prev_blockhash = common.block_hash();
        fork.time = 1_600_000_000 + 1_209_600;
        let mut fork_work = store_node(&mut tree, 2015, fork, common_work, false);
        let mut next = fork;
        next.prev_blockhash = fork.block_hash();
        next.time += 600;
        fork = next;
        fork_work = store_node(&mut tree, 2016, fork, fork_work, false);
        assert!(fork_work < old_work);
        tree.candidate_forks.push(Tip {
            hash: fork.block_hash(),
            height: 2016,
            next_work_required: Some(hard),
        });

        let mut winning = fork;
        winning.prev_blockhash = fork.block_hash();
        winning.time += 600;
        assert!(fork_work + winning.work() > old_work);
        let disconnected = match tree.accept_header(winning) {
            AcceptHeaderChanges::Reorganization { disconnected, .. } => disconnected,
            other => panic!("expected a shorter, higher-work reorganization: {other:?}"),
        };
        assert_eq!(tree.height(), 2017);
        let old_tail_hash = old_branch.last().unwrap().block_hash();
        assert_eq!(
            tree.height_of_hash_canonical_only(old_tail_hash),
            None,
            "the displaced height-2020 block is still advertised as canonical"
        );
        for height in 2018..=2020 {
            assert_eq!(tree.block_hash_at_height(height), None);
            assert_eq!(tree.header_at_height(height), None);
        }
        assert_eq!(disconnected.len(), old_branch.len());
        for header in old_branch {
            assert!(disconnected.iter().any(|entry| entry.header == header));
        }
    }

    fn mine_child(parent: Header, delay: u32, bits: CompactTarget) -> Header {
        let mut child = parent;
        child.prev_blockhash = parent.block_hash();
        child.time += delay;
        child.bits = bits;
        child.nonce = 0;
        while child.validate_pow(child.target()).is_err() {
            child.nonce += 1;
        }
        child
    }

    #[test]
    fn test_rejected_fork_extension_keeps_fork() {
        let mut tree = BlockTree::from_genesis(Network::Regtest);
        let genesis = genesis_block(Network::Regtest).header;
        let bits = genesis.bits;
        let common = mine_child(genesis, 600, bits);
        let a2 = mine_child(common, 600, bits);
        let a3 = mine_child(a2, 600, bits);
        let b2 = mine_child(common, 601, bits);
        let b3 = mine_child(b2, 600, bits);
        for header in [common, a2, a3, b2, b3] {
            tree.accept_header(header);
        }
        assert_eq!(tree.tip_hash(), a3.block_hash());

        let invalid = mine_child(b3, 600, CompactTarget::from_consensus(0x2100ffff));
        assert!(matches!(
            tree.accept_header(invalid),
            AcceptHeaderChanges::Rejected(HeaderRejection::InvalidPow { .. })
        ));
        assert_eq!(tree.tip_hash(), a3.block_hash());

        let b4 = mine_child(b3, 601, bits);
        assert!(matches!(
            tree.accept_header(b4),
            AcceptHeaderChanges::Reorganization { .. }
        ));
        assert_eq!(tree.tip_hash(), b4.block_hash());
    }
}
