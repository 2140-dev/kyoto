use std::collections::{BTreeMap, HashMap};

use crate::HeaderCheckpoint;

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

impl From<HeaderCheckpoint> for Tip {
    fn from(value: HeaderCheckpoint) -> Self {
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
}

impl BlockNode {
    fn new(height: Height, header: Header, acc_work: Work) -> Self {
        Self {
            height,
            header,
            acc_work,
            filter_commitment: None,
            filter_checked: false,
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
            let fork = self.candidate_forks.swap_remove(fork_index);
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
                                }
                                *canonical_hash = curr_hash;
                                connections.push(IndexedHeader::new(node.height, node.header));
                                curr_hash = next;
                            } else {
                                return (connections, disconnections);
                            }
                        }
                        None => {
                            self.canonical_hashes.insert(node.height, curr_hash);
                            connections.push(IndexedHeader::new(node.height, node.header));
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
            node.filter_commitment = Some(commitment)
        }
    }

    pub(crate) fn check_filter(&mut self, hash: BlockHash) {
        if let Some(node) = self.headers.get_mut(&hash) {
            node.filter_checked = true
        }
    }

    pub(crate) fn assume_checked_to(&mut self, assumed_height: Height) {
        let mut curr = self.tip_hash();
        while let Some(node) = self.headers.get_mut(&curr) {
            if node.height <= assumed_height {
                node.filter_checked = true
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

    pub(crate) fn reset_all_filters(&mut self) {
        let mut curr = self.tip_hash();
        while self.headers.get_mut(&curr).is_some() {
            match self.headers.get_mut(&curr) {
                Some(node) => {
                    node.filter_checked = false;
                    curr = node.header.prev_blockhash;
                }
                None => break,
            }
        }
        for fork in &self.candidate_forks {
            curr = fork.hash;
            while self.headers.get_mut(&curr).is_some() {
                match self.headers.get_mut(&curr) {
                    Some(node) => {
                        if !node.filter_checked {
                            break;
                        }
                        node.filter_checked = false;
                        curr = node.header.prev_blockhash;
                    }
                    None => break,
                }
            }
        }
    }

    pub(crate) fn filter_headers_synced(&self) -> bool {
        self.iter_data()
            .map(|node| node.filter_commitment)
            .all(|commitment| commitment.is_some())
    }

    pub(crate) fn filters_synced(&self) -> bool {
        self.iter_data().all(|node| node.filter_checked)
    }

    pub(crate) fn total_filters_synced(&self) -> u32 {
        self.iter_data().filter(|node| node.filter_checked).count() as u32
    }

    pub(crate) fn total_filter_headers_synced(&self) -> u32 {
        self.iter_data()
            .filter(|node| node.filter_commitment.is_some())
            .count() as u32
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
        chain.assume_checked_to(4);
        assert!(chain.filters_synced());
    }
}
