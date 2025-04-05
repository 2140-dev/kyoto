use std::collections::{BTreeMap, HashMap};

use crate::{prelude::ZerolikeExt, HeaderCheckpoint};

use bitcoin::{
    block::Header, constants::genesis_block, params::Params, BlockHash, CompactTarget, FilterHash,
    Network, Work,
};

use super::IndexedHeader;

const LOCATOR_INDEX: &[u32] = &[1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Height(u32);

impl Height {
    fn new(height: u32) -> Self {
        Height(height)
    }

    fn from_u64_checked(height: u64) -> Option<Self> {
        match height.try_into() {
            Ok(height) => Some(Height::new(height)),
            Err(_) => None,
        }
    }

    fn increment(&self) -> Self {
        Self(self.0 + 1)
    }

    fn checked_sub(&self, other: impl Into<Height>) -> Option<Self> {
        let other = other.into();
        let height_sub_checked = self.0.checked_sub(other.0);
        height_sub_checked.map(Self)
    }

    fn is_adjustment_multiple(&self, params: impl AsRef<Params>) -> bool {
        self.0 as u64 % params.as_ref().difficulty_adjustment_interval() == 0
    }

    pub(crate) fn to_u32(self) -> u32 {
        self.0
    }

    #[allow(dead_code)]
    fn to_u64(self) -> u64 {
        self.0 as u64
    }
}

impl From<u32> for Height {
    fn from(value: u32) -> Self {
        Height(value)
    }
}

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
    pub(crate) fn from_checkpoint(height: impl Into<Height>, hash: BlockHash) -> Self {
        let height = height.into();
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

#[derive(Debug, Clone, Hash)]
pub(crate) struct BlockNode {
    pub height: Height,
    pub header: Header,
    pub acc_work: Work,
    pub filter_hash: Option<FilterHash>,
    pub filter_checked: bool,
}

impl BlockNode {
    fn new(height: Height, header: Header, acc_work: Work) -> Self {
        Self {
            height,
            header,
            acc_work,
            filter_hash: None,
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
        let height = Height::new(0);
        let hash = genesis.block_hash();
        let tip = Tip {
            hash,
            height,
            next_work_required: Some(genesis.header.bits),
        };
        let mut headers = HashMap::with_capacity(20_000);
        let block_node = BlockNode::new(height, genesis.header, genesis.header.work());
        headers.insert(hash, block_node);
        let mut canonical_hashes = BTreeMap::new();
        canonical_hashes.insert(Height::new(0), hash);
        Self {
            canonical_hashes,
            headers,
            active_tip: tip,
            candidate_forks: Vec::with_capacity(2),
            network,
        }
    }

    pub(crate) fn from_header(height: impl Into<Height>, header: Header, network: Network) -> Self {
        let height = height.into();
        let hash = header.block_hash();
        let tip = Tip {
            hash,
            height,
            next_work_required: Some(header.bits),
        };
        let mut headers = HashMap::with_capacity(20_000);
        let block_node = BlockNode::new(height, header, header.work());
        headers.insert(hash, block_node);
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
                connected_at: IndexedHeader::new(new_height.to_u32(), new_header),
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
                        connected_at: IndexedHeader::new(new_height.to_u32(), new_header),
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
                    connected_at: IndexedHeader::new(new_height.to_u32(), new_header),
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
                                    disconnections.push(IndexedHeader::new(
                                        reorged.height.to_u32(),
                                        reorged.header,
                                    ));
                                }
                                *canonical_hash = curr_hash;
                                connections
                                    .push(IndexedHeader::new(node.height.to_u32(), node.header));
                                curr_hash = next;
                            } else {
                                return (connections, disconnections);
                            }
                        }
                        None => {
                            self.canonical_hashes.insert(node.height, curr_hash);
                            connections.push(IndexedHeader::new(node.height.to_u32(), node.header));
                            curr_hash = next;
                        }
                    }
                }
                None => return (connections, disconnections),
            }
        }
    }

    fn compute_next_work_required(&self, new_height: Height) -> Option<CompactTarget> {
        let adjustment_period =
            Height::from_u64_checked(self.network.params().difficulty_adjustment_interval())?;
        let epoch_start = new_height.checked_sub(adjustment_period)?;
        let epoch_end = new_height.checked_sub(Height::new(1))?;
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

    pub(crate) fn block_hash_at_height(&self, height: impl Into<Height>) -> Option<BlockHash> {
        let height = height.into();
        if self.active_tip.height.eq(&height) {
            return Some(self.active_tip.hash);
        }
        self.canonical_hashes.get(&height).copied()
    }

    pub(crate) fn header_at_height(&self, height: impl Into<Height>) -> Option<Header> {
        let height = height.into();
        let hash = self.canonical_hashes.get(&height)?;
        self.headers.get(hash).map(|node| node.header)
    }

    pub(crate) fn height_of_hash(&self, hash: BlockHash) -> Option<u32> {
        self.headers.get(&hash).map(|node| node.height.to_u32())
    }

    pub(crate) fn height(&self) -> u32 {
        self.active_tip.height.to_u32()
    }

    pub(crate) fn contains(&self, hash: BlockHash) -> bool {
        self.headers.contains_key(&hash) || self.active_tip.hash.eq(&hash)
    }

    pub(crate) fn tip_hash(&self) -> BlockHash {
        self.active_tip.hash
    }

    pub(crate) fn filter_hash(&self, block_hash: BlockHash) -> Option<FilterHash> {
        self.headers.get(&block_hash)?.filter_hash
    }

    pub(crate) fn filter_hash_at_height(&self, height: impl Into<Height>) -> Option<FilterHash> {
        let height = height.into();
        let hash = self.canonical_hashes.get(&height)?;
        self.headers.get(hash)?.filter_hash
    }

    pub(crate) fn all_filters_checked(&self) -> bool {
        self.iter_filters_checked().all(|checked| checked)
    }

    pub(crate) fn filter_checked(&mut self, hash: BlockHash) {
        if let Some(data) = self.headers.get_mut(&hash) {
            data.filter_checked = true
        }
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

    pub(crate) fn internally_cached_headers(&self) -> usize {
        self.headers.len()
    }

    pub(crate) fn iter_headers(&self) -> BlockHeaderIterator {
        BlockHeaderIterator {
            block_tree: self,
            current: self.active_tip.hash,
        }
    }

    pub(crate) fn iter_data(&self) -> BlockNodeIterator {
        BlockNodeIterator {
            block_tree: self,
            current: self.active_tip.hash,
        }
    }

    pub(crate) fn iter_filters_checked(&self) -> FilterCheckedIterator {
        FilterCheckedIterator {
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
        Some(IndexedHeader::new(node.height.to_u32(), node.header))
    }
}

pub(crate) struct FilterCheckedIterator<'a> {
    block_tree: &'a BlockTree,
    current: BlockHash,
}

impl Iterator for FilterCheckedIterator<'_> {
    type Item = bool;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.block_tree.headers.get(&self.current)?;
        self.current = node.header.prev_blockhash;
        Some(node.filter_checked)
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
    use bitcoin::consensus::deserialize;

    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_depth_one_reorg() {
        let block_8: Header = deserialize(&hex::decode("0000002016fe292517eecbbd63227d126a6b1db30ebc5262c61f8f3a4a529206388fc262dfd043cef8454f71f30b5bbb9eb1a4c9aea87390f429721e435cf3f8aa6e2a9171375166ffff7f2000000000").unwrap()).unwrap();
        let block_9: Header = deserialize(&hex::decode("000000205708a90197d93475975545816b2229401ccff7567cb23900f14f2bd46732c605fd8de19615a1d687e89db365503cdf58cb649b8e935a1d3518fa79b0d408704e71375166ffff7f2000000000").unwrap()).unwrap();
        let block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093790c9f554a7780a6043a19619d2a4697364bb62abf6336c0568c31f1eedca3c3e171375166ffff7f2000000000").unwrap()).unwrap();
        // let batch_1 = vec![block_8, block_9, block_10];
        let new_block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093792151c0e9ce4e4c789ca98427d7740cc7acf30d2ca0c08baef266bf152289d814567e5e66ffff7f2001000000").unwrap()).unwrap();
        let block_11: Header = deserialize(&hex::decode("00000020efcf8b12221fccc735b9b0b657ce15b31b9c50aff530ce96a5b4cfe02d8c0068496c1b8a89cf5dec22e46c35ea1035f80f5b666a1b3aa7f3d6f0880d0061adcc567e5e66ffff7f2001000000").unwrap()).unwrap();
        // let fork = vec![new_block_10, block_11];

        let tip = Tip::from_checkpoint(
            7,
            BlockHash::from_str("62c28f380692524a3a8f1fc66252bc0eb31d6b6a127d2263bdcbee172529fe16")
                .unwrap(),
        );
        let mut chain = BlockTree::new(tip, Network::Regtest);
        let accept_8 = chain.accept_header(block_8);
        assert!(matches!(
            accept_8,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_9 = chain.accept_header(block_9);
        assert!(matches!(
            accept_9,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_10 = chain.accept_header(block_10);
        assert!(matches!(
            accept_10,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        assert_eq!(chain.height(), 10);
        let accept_new_10 = chain.accept_header(new_block_10);
        assert!(matches!(
            accept_new_10,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 10);
        assert_eq!(chain.header_at_height(10), Some(block_10));
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
                assert_eq!(block_10, disconnected.first().unwrap().header);
                assert_eq!(10, disconnected.first().unwrap().height);
                assert_eq!(1, disconnected.len())
            }
            _ => panic!("reorganization should have been accepted"),
        }
        assert_eq!(chain.header_at_height(12), None);
        assert_eq!(chain.header_at_height(11), Some(block_11));
        assert_eq!(chain.header_at_height(10), Some(new_block_10));
        assert_eq!(chain.header_at_height(9), Some(block_9));
        assert_eq!(chain.header_at_height(8), Some(block_8));
    }

    #[test]
    fn test_depth_two_reorg() {
        let block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f047eb4d0fe76345e307d0e020a079cedfa37101ee7ac84575cf829a611b0f84bc4805e66ffff7f2001000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("00000020299e41732deb76d869fcdb5f72518d3784e99482f572afb73068d52134f1f75e1f20f5da8d18661d0f13aa3db8fff0f53598f7d61f56988a6d66573394b2c6ffc5805e66ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea3471f1dbedc283ce6a43a87ed3c8e6326dae8d3dbacce1b2daba08e508054ffdb697815e66ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002052ff614fa461ff38b4a5c101d04fdcac2f34722e60bd43d12c8de0a394fe0c60444fb24b7e9885f60fed9927d27f23854ecfab29287163ef2b868d5d626f82ed97815e66ffff7f2002000000").unwrap()).unwrap();
        let new_block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea349c6240c5d0521966771808950f796c9c04088bc9551a828b64f1cf06831705dfbc835e66ffff7f2000000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("00000020d2a1c6ba2be393f405fe2f4574565f9ee38ac68d264872fcd82b030970d0232ce882eb47c3dd138587120f1ad97dd0e73d1e30b79559ad516cb131f83dcb87e9bc835e66ffff7f2002000000").unwrap()).unwrap();
        let mut chain = BlockTree::from_genesis(Network::Regtest);
        let accept_1 = chain.accept_header(block_1);
        assert!(matches!(
            accept_1,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_2 = chain.accept_header(block_2);
        assert!(matches!(
            accept_2,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_3 = chain.accept_header(block_3);
        assert!(matches!(
            accept_3,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_4 = chain.accept_header(block_4);
        assert!(matches!(
            accept_4,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        assert_eq!(chain.height(), 4);
        // Create a new fork
        let accept_new_3 = chain.accept_header(new_block_3);
        assert!(matches!(
            accept_new_3,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 4);
        assert_eq!(chain.header_at_height(3), Some(block_3));
        // Advertise the same fork
        let accept_new_3 = chain.accept_header(new_block_3);
        assert!(matches!(accept_new_3, AcceptHeaderChanges::Duplicate));
        assert_eq!(chain.height(), 4);
        // Extend the fork, but do not switch to it
        let accept_new_4 = chain.accept_header(new_block_4);
        assert!(matches!(
            accept_new_4,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        assert_eq!(chain.height(), 4);
        assert_eq!(chain.header_at_height(4), Some(block_4));
        assert_eq!(chain.header_at_height(3), Some(block_3));
        assert_eq!(chain.header_at_height(2), Some(block_2));
        assert_eq!(chain.header_at_height(1), Some(block_1));
    }

    #[test]
    fn test_reorg_to_root() {
        let block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f575b313ad3ef825cfc204c34da8f3c1fd1784e2553accfa38001010587cb57241f855e66ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("00000020c81cedd6a989939936f31448e49d010a13c2e750acf02d3fa73c9c7ecfb9476e798da2e5565335929ad303fc746acabc812ee8b06139bcf2a4c0eb533c21b8c420855e66ffff7f2000000000").unwrap()).unwrap();
        // batch_1 = vec![block_1, block_2];
        let new_block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f575b313ad3ef825cfc204c34da8f3c1fd1784e2553accfa38001010587cb5724d5855e66ffff7f2004000000").unwrap()).unwrap();
        let new_block_2: Header = deserialize(&hex::decode("00000020d1d80f53343a084bd0da6d6ab846f9fe4a133de051ea00e7cae16ed19f601065798da2e5565335929ad303fc746acabc812ee8b06139bcf2a4c0eb533c21b8c4d6855e66ffff7f2000000000").unwrap()).unwrap();
        // fork = vec![new_block_1, new_block_2];
        let block_3: Header = deserialize(&hex::decode("0000002080f38c14e898d6646dd426428472888966e0d279d86453f42edc56fdb143241aa66c8fa8837d95be3f85d53f22e86a0d6d456b1ab348e073da4d42a39f50637423865e66ffff7f2000000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("000000204877fed370af64c0a1f7a76f6944e1127aad965b1865f99ecfdf8fa72ae23377f51921d01ff1131bd589500a8ca142884297ceeb1aa762ad727249e9a23f2cb023865e66ffff7f2000000000").unwrap()).unwrap();
        // batch_2 = vec![block_3, block_4];
        let mut chain = BlockTree::from_genesis(Network::Regtest);
        let accept_1 = chain.accept_header(block_1);
        assert!(matches!(
            accept_1,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        let accept_2 = chain.accept_header(block_2);
        assert!(matches!(
            accept_2,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
        assert_eq!(chain.height(), 2);
        let fork_1 = chain.accept_header(new_block_1);
        assert!(matches!(
            fork_1,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        let fork_2 = chain.accept_header(new_block_2);
        assert!(matches!(
            fork_2,
            AcceptHeaderChanges::ExtendedFork { connected_at: _ }
        ));
        let reorg_1 = chain.accept_header(block_3);
        match reorg_1 {
            AcceptHeaderChanges::Reorganization {
                accepted,
                disconnected,
            } => {
                assert_eq!(
                    accepted,
                    vec![
                        IndexedHeader::new(3, block_3),
                        IndexedHeader::new(2, new_block_2),
                        IndexedHeader::new(1, new_block_1),
                    ]
                );
                assert_eq!(
                    disconnected,
                    vec![
                        IndexedHeader::new(2, block_2),
                        IndexedHeader::new(1, block_1),
                    ]
                );
            }
            _ => panic!("reorganization should have been accepted"),
        }
        let accept_4 = chain.accept_header(block_4);
        assert!(matches!(
            accept_4,
            AcceptHeaderChanges::Accepted { connected_at: _ }
        ));
    }
}
