extern crate alloc;
use std::{collections::BTreeMap, sync::Arc};

use bitcoin::{
    block::Header,
    p2p::message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
    BlockHash, Network,
};
use tokio::sync::Mutex;

use super::{
    error::{CFHeaderSyncError, CFilterSyncError, HeaderSyncError},
    graph::{AcceptHeaderChanges, BlockTree, HeaderRejection},
    CFHeaderBatch, CFHeaderChanges, ChainState, Filter, FilterCheck, FilterHeaderRequest,
    FilterRequest, FilterRequestState, HeaderValidationExt, HeightMonitor, PeerId,
};
use crate::{chain::BlockHeaderChanges, messages::Event, Dialog, Info, Progress};
use crate::{FilterType, IndexedFilter};

const CF_HEADER_BATCH_SIZE: u32 = 1_999;
const FILTER_BATCH_SIZE: u32 = 999;

#[derive(Debug)]
pub(crate) struct Chain {
    pub(crate) header_chain: BlockTree,
    request_state: FilterRequestState,
    network: Network,
    heights: Arc<Mutex<HeightMonitor>>,
    dialog: Arc<Dialog>,
    filter_type: FilterType,
}

impl Chain {
    pub(crate) fn new(
        network: Network,
        chain_state: ChainState,
        dialog: Arc<Dialog>,
        height_monitor: Arc<Mutex<HeightMonitor>>,
        quorum_required: u8,
        filter_type: FilterType,
    ) -> Self {
        let header_chain = match chain_state {
            ChainState::Snapshot(headers) => {
                let mut header_iter = headers.into_iter();
                match header_iter.next() {
                    Some(header) => {
                        let mut block_tree = BlockTree::new(header, network);
                        for rest in header_iter {
                            let _ = block_tree.accept_header(rest.header);
                        }
                        block_tree
                    }
                    None => BlockTree::from_genesis(network),
                }
            }
            ChainState::Checkpoint(cp) => BlockTree::new(cp, network),
        };
        Chain {
            header_chain,
            request_state: FilterRequestState::new(quorum_required),
            network,
            heights: height_monitor,
            dialog,
            filter_type,
        }
    }

    // The last ten heights and headers in the chain
    pub(crate) fn last_ten(&self) -> BTreeMap<u32, Header> {
        self.header_chain
            .iter_headers()
            .take(10)
            .map(|index| (index.height, index.header))
            .collect()
    }

    // Do we have best known height and is our height equal to it
    // If our height is greater, we received partial inventory, and
    // the header message contained the rest of the new blocks.
    pub(crate) async fn is_synced(&self) -> bool {
        let height_lock = self.heights.lock().await;
        match height_lock.max() {
            Some(peer_max) => self.header_chain.height() >= peer_max,
            None => false,
        }
    }

    // Sync the chain with headers from a peer, adjusting to reorgs if needed
    pub(crate) fn sync_chain(
        &mut self,
        header_batch: Vec<Header>,
    ) -> Result<Vec<BlockHash>, HeaderSyncError> {
        if header_batch.is_empty() {
            return Err(HeaderSyncError::EmptyMessage);
        }
        // If our chain already has the last header in the message there is no new information
        if self.header_chain.contains(
            header_batch
                .last()
                .expect("non-empty check above.")
                .block_hash(),
        ) {
            return Ok(Vec::new());
        }
        // We check first if the peer is sending us nonsense
        self.sanity_check(&header_batch)?;
        let mut reorgs = Vec::new();
        for header in header_batch.into_iter() {
            let changes = self.header_chain.accept_header(header);
            match changes {
                AcceptHeaderChanges::Accepted { connected_at } => {
                    self.dialog
                        .send_event(Event::ChainUpdate(BlockHeaderChanges::Connected(
                            connected_at,
                        )));
                }
                AcceptHeaderChanges::Duplicate => (),
                AcceptHeaderChanges::ExtendedFork { connected_at } => {
                    crate::debug!(format!("Fork created or extended {}", connected_at.height));
                    self.dialog
                        .send_event(Event::ChainUpdate(BlockHeaderChanges::ForkAdded(
                            connected_at,
                        )));
                }
                AcceptHeaderChanges::Reorganization {
                    mut accepted,
                    mut disconnected,
                } => {
                    crate::debug!("Valid reorganization found");
                    accepted.sort();
                    disconnected.sort();
                    reorgs = disconnected
                        .clone()
                        .into_iter()
                        .map(|header| header.block_hash())
                        .collect::<Vec<BlockHash>>();
                    self.clear_compact_filter_queue();
                    let disconnected_event = Event::ChainUpdate(BlockHeaderChanges::Reorganized {
                        accepted,
                        reorganized: disconnected,
                    });
                    self.dialog.send_event(disconnected_event);
                }
                AcceptHeaderChanges::Rejected(rejected_header) => match rejected_header {
                    HeaderRejection::InvalidPow {
                        expected: _,
                        got: _,
                    } => return Err(HeaderSyncError::InvalidBits),
                    HeaderRejection::UnknownPrevHash(_) => {
                        crate::debug!("Unknown prevhash does not link to the current header chain");
                        return Err(HeaderSyncError::FloatingHeaders);
                    }
                },
            }
        }
        Ok(reorgs)
    }

    // These are invariants in all batches of headers we receive
    fn sanity_check(&mut self, header_batch: &[Header]) -> Result<(), HeaderSyncError> {
        if !header_batch.connected() {
            return Err(HeaderSyncError::HeadersNotConnected);
        }
        if !header_batch.passes_own_pow() {
            return Err(HeaderSyncError::InvalidHeaderWork);
        }
        if !header_batch.bits_adhere_transition_threshold(self.network) {
            return Err(HeaderSyncError::InvalidBits);
        }
        Ok(())
    }

    // Sync the compact filter headers, possibly encountering conflicts
    pub(crate) fn sync_cf_headers(
        &mut self,
        peer_id: PeerId,
        cf_headers: CFHeaders,
    ) -> Result<CFHeaderChanges, CFHeaderSyncError> {
        let batch: CFHeaderBatch = cf_headers.into();
        let request = self
            .request_state
            .last_filter_header_request
            .ok_or(CFHeaderSyncError::UnexpectedCFHeaderMessage)?;
        if let Some(expected) = request.expected_prev_filter_header {
            if expected.ne(batch.prev_header()) {
                // This is an older message and we already have the headers corresponding to this
                // message
                if self
                    .header_chain
                    .iter_data()
                    .filter_map(|node| node.filter_commitment)
                    .any(|commit| commit.header.eq(batch.prev_header()))
                {
                    return Ok(CFHeaderChanges::AddedToQueue);
                } else {
                    return Err(CFHeaderSyncError::PrevHeaderMismatch);
                }
            }
        }
        if request.stop_hash.ne(&batch.stop_hash()) {
            return Err(CFHeaderSyncError::UnrequestedStophash);
        }
        // Check that the start height we requested and the length of the batch are aligned.
        let height_of_stop_hash = self
            .header_chain
            .height_of_hash(batch.stop_hash())
            .ok_or(CFHeaderSyncError::UnknownStophash)?;
        let offset = batch
            .len()
            .checked_sub(1)
            .ok_or(CFHeaderSyncError::EmptyMessage)?;
        let expected_start_height = height_of_stop_hash
            .checked_sub(offset)
            .ok_or(CFHeaderSyncError::HeaderChainIndexOverflow)?;
        if expected_start_height.ne(&request.start_height) {
            return Err(CFHeaderSyncError::StartHeightMisalignment);
        }

        match self.request_state.pending_batch.take() {
            Some((id, pending)) => {
                if peer_id.eq(&id) {
                    return Ok(CFHeaderChanges::AddedToQueue);
                }
                if pending.ne(&batch) {
                    self.request_state.pending_batch = None;
                    self.request_state.agreement_state.reset_agreements();
                    Ok(CFHeaderChanges::Conflict)
                } else {
                    self.request_state.agreement_state.got_agreement();
                    if self.request_state.agreement_state.enough_agree() {
                        self.request_state.agreement_state.reset_agreements();
                        self.push_cf_header_batch(batch, request.stop_hash);
                        Ok(CFHeaderChanges::Extended)
                    } else {
                        self.request_state.pending_batch = Some((id, batch));
                        Ok(CFHeaderChanges::AddedToQueue)
                    }
                }
            }
            None => {
                self.request_state.agreement_state.got_agreement();
                if self.request_state.agreement_state.enough_agree() {
                    self.request_state.agreement_state.reset_agreements();
                    self.push_cf_header_batch(batch, request.stop_hash);
                    Ok(CFHeaderChanges::Extended)
                } else {
                    self.request_state.pending_batch = Some((peer_id, batch));
                    Ok(CFHeaderChanges::AddedToQueue)
                }
            }
        }
    }

    fn push_cf_header_batch(&mut self, mut batch: CFHeaderBatch, stop_hash: BlockHash) {
        // Start from the stop hash and work backwards
        let cf_header_iter = batch.take_inner().into_iter().rev();
        let mut curr = stop_hash;
        for commitment in cf_header_iter {
            self.header_chain.set_commitment(commitment, curr);
            match self.header_chain.header_at_hash(curr) {
                Some(header) => {
                    curr = header.prev_blockhash;
                }
                // This is not expected to happen, as to request this filter header in the first place,
                // this header had to exist in the header chain.
                None => break,
            }
        }
    }

    // We need to make this public for new peers that connect to us throughout syncing the filter headers
    pub(crate) fn next_cf_header_message(&mut self) -> GetCFHeaders {
        let mut last_unchecked_cfheader = self.header_chain.height();
        let mut prev_header = None;
        for data in self.header_chain.iter_data() {
            if data.height.eq(&0) {
                break;
            }
            match data.filter_commitment {
                Some(commitment) => {
                    prev_header = Some(commitment.header);
                    break;
                }
                None => {
                    last_unchecked_cfheader = data.height;
                }
            }
        }
        let stop_hash_index = last_unchecked_cfheader + CF_HEADER_BATCH_SIZE;
        let stop_hash = self
            .header_chain
            .block_hash_at_height(stop_hash_index)
            .unwrap_or(self.header_chain.tip_hash());
        self.request_state.last_filter_header_request = Some(FilterHeaderRequest {
            expected_prev_filter_header: prev_header,
            start_height: last_unchecked_cfheader,
            stop_hash,
        });
        GetCFHeaders {
            filter_type: self.filter_type.into(),
            start_height: last_unchecked_cfheader,
            stop_hash,
        }
    }

    // Are the compact filter headers caught up to the header chain
    pub(crate) fn is_cf_headers_synced(&self) -> bool {
        self.header_chain.filter_headers_synced()
    }

    // Handle a new filter
    pub(crate) fn sync_filter(
        &mut self,
        filter_message: CFilter,
    ) -> Result<FilterCheck, CFilterSyncError> {
        let filter = Filter::new(filter_message.filter, filter_message.block_hash);
        if self
            .header_chain
            .is_filter_checked(&filter_message.block_hash)
        {
            return Ok(FilterCheck {
                was_last_in_batch: false,
            });
        }
        let expected_filter_hash = self
            .header_chain
            .filter_commitment(filter_message.block_hash);
        // Disallow any filter that we do not have a block hash for
        match expected_filter_hash {
            Some(ref_hash) => {
                if filter.filter_hash().ne(&ref_hash.filter_hash) {
                    return Err(CFilterSyncError::MisalignedFilterHash);
                }
            }
            None => {
                return Err(CFilterSyncError::UnknownFilterHash);
            }
        }
        let height = self
            .header_chain
            .height_of_hash(filter_message.block_hash)
            .ok_or(CFilterSyncError::UnknownFilterHash)?;
        let indexed_filter = IndexedFilter::new(height, filter);
        self.dialog.send_event(Event::IndexedFilter(indexed_filter));
        self.header_chain.check_filter(filter_message.block_hash);
        let stop_hash = self
            .request_state
            .last_filter_request
            .ok_or(CFilterSyncError::UnrequestedStophash)?
            .stop_hash;
        let was_last_in_batch = filter_message.block_hash.eq(&stop_hash);
        Ok(FilterCheck { was_last_in_batch })
    }

    // Next filter message, if there is one
    pub(crate) fn next_filter_message(&mut self) -> GetCFilters {
        let mut last_unchecked_filter = self.header_chain.height();
        for block_data in self.header_chain.iter_data() {
            if block_data.height.eq(&0) {
                break;
            }
            if block_data.filter_checked {
                break;
            }
            last_unchecked_filter = block_data.height;
        }
        let stop_hash_index = last_unchecked_filter + FILTER_BATCH_SIZE;
        let stop_hash = self
            .header_chain
            .block_hash_at_height(stop_hash_index)
            .unwrap_or(self.header_chain.tip_hash());
        self.request_state.last_filter_request = Some(FilterRequest {
            stop_hash,
            start_height: last_unchecked_filter,
        });
        GetCFilters {
            filter_type: self.filter_type.into(),
            start_height: last_unchecked_filter,
            stop_hash,
        }
    }

    // Are we synced with filters
    pub(crate) fn is_filters_synced(&self) -> bool {
        self.header_chain.filters_synced()
    }

    // Reset the compact filter queue because we received a new block
    pub(crate) fn clear_compact_filter_queue(&mut self) {
        self.request_state.agreement_state.reset_agreements();
        self.request_state.last_filter_header_request = None;
        self.request_state.pending_batch = None;
    }

    // Clear the filter header cache to rescan the filters for new scripts.
    pub(crate) fn clear_filters(&mut self) {
        self.header_chain.reset_all_filters();
    }

    pub(crate) async fn send_chain_update(&self) {
        self.dialog
            .send_info(Info::Progress(Progress::new(
                self.header_chain.total_filter_headers_synced(),
                self.header_chain.total_filters_synced(),
                self.header_chain.internal_chain_len() as u32,
                self.header_chain.height(),
            )))
            .await;
        crate::debug!(format!(
            "Headers: ({}/{}) CFHeaders: ({}/{}) CFilters: ({}/{})",
            self.header_chain.height(),
            self.heights
                .lock()
                .await
                .max()
                .unwrap_or(self.header_chain.height()),
            self.header_chain.total_filter_headers_synced(),
            self.header_chain.internal_chain_len() as u32,
            self.header_chain.total_filters_synced(),
            self.header_chain.internal_chain_len() as u32,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::{fs::File, str::FromStr};

    use bitcoin::hashes::sha256d;
    use bitcoin::hashes::Hash;
    use bitcoin::{
        block::Header,
        consensus::deserialize,
        p2p::message_filter::{CFHeaders, CFilter},
        BlockHash, FilterHash, FilterHeader,
    };
    use corepc_node::serde_json;
    use tokio::sync::Mutex;

    use crate::chain::ChainState;
    use crate::FilterType;
    use crate::{
        chain::checkpoints::HeaderCheckpoint,
        messages::{Event, Info, Warning},
        Dialog,
    };

    use super::{CFHeaderChanges, Chain, HeightMonitor};

    fn new_regtest(
        anchor: HeaderCheckpoint,
        height_monitor: Arc<Mutex<HeightMonitor>>,
        peers: u8,
    ) -> Chain {
        let (info_tx, _) = tokio::sync::mpsc::channel::<Info>(1);
        let (warn_tx, _) = tokio::sync::mpsc::unbounded_channel::<Warning>();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel::<Event>();
        Chain::new(
            bitcoin::Network::Regtest,
            ChainState::Checkpoint(anchor),
            Arc::new(Dialog::new(info_tx, warn_tx, event_tx)),
            height_monitor,
            peers,
            FilterType::Basic,
        )
    }

    fn base_block() -> HeaderCheckpoint {
        HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        )
    }

    #[derive(Debug, Clone)]
    struct HexHeader(Header);
    crate::impl_deserialize!(HexHeader, Header);

    #[derive(Debug, Clone)]
    struct HexFilter(Vec<u8>);

    impl<'de> serde::Deserialize<'de> for HexFilter {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let s = String::deserialize(deserializer)?;
            let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
            Ok(HexFilter(bytes))
        }
    }

    #[derive(Debug, Clone)]
    struct HexPrevHeader(FilterHeader);
    crate::impl_deserialize!(HexPrevHeader, FilterHeader);

    #[derive(Debug, Clone, serde::Deserialize)]
    struct BlockData {
        header: HexHeader,
        filter: HexFilter,
    }

    impl BlockData {
        fn filter_hash(&self) -> FilterHash {
            let hash = sha256d::Hash::hash(&self.filter.0);
            FilterHash::from_raw_hash(hash)
        }
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct ChainScenario {
        most_work: Vec<BlockData>,
        stale_chain: Vec<BlockData>,
        prev_header: HexPrevHeader,
    }

    impl ChainScenario {
        fn most_work_headers(&self) -> Vec<Header> {
            self.most_work.iter().map(|data| data.header.0).collect()
        }

        fn n_most_work_headers(&self, n: usize) -> Vec<Header> {
            self.most_work
                .iter()
                .map(|data| data.header.0)
                .take(n)
                .collect()
        }

        fn filters(&self) -> Vec<CFilter> {
            self.most_work
                .iter()
                .map(|data| CFilter {
                    filter_type: 0x00,
                    block_hash: data.header.0.block_hash(),
                    filter: data.filter.0.clone(),
                })
                .collect()
        }

        fn n_most_work_filter_hashes(&self, n: usize) -> Vec<FilterHash> {
            self.most_work
                .iter()
                .take(n)
                .map(|data| data.filter_hash())
                .collect()
        }

        fn last_block_hash(&self) -> BlockHash {
            self.most_work
                .last()
                .map(|data| data.header.0.block_hash())
                .unwrap()
        }

        fn last_block_header(&self) -> Header {
            self.most_work.last().map(|data| data.header.0).unwrap()
        }

        fn prev_header(&self) -> FilterHeader {
            self.prev_header.0
        }
    }

    fn load_scenario() -> ChainScenario {
        let file = File::open("./tests/data/chain_scenario_one.json").unwrap();
        serde_json::from_reader(&file).unwrap()
    }

    #[tokio::test]
    async fn test_fork_includes_old_vals() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor, 1);
        let chain_scenario = load_scenario();
        let canonical = chain_scenario.most_work;
        let mut canonical_iter = canonical.into_iter();
        let block_1 = canonical_iter.next().unwrap().header.0;
        let block_2 = canonical_iter.next().unwrap().header.0;
        let block_3 = canonical_iter.next().unwrap().header.0;
        let block_4 = chain_scenario
            .stale_chain
            .into_iter()
            .next()
            .unwrap()
            .header
            .0;
        let batch_1 = vec![block_1, block_2, block_3, block_4];
        let new_block_4 = canonical_iter.next().unwrap().header.0;
        let block_5 = canonical_iter.next().unwrap().header.0;
        let batch_2 = vec![block_1, block_2, block_3, new_block_4, block_5];
        let chain_sync = chain.sync_chain(batch_1);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        let chain_sync = chain.sync_chain(batch_2);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        let block_iter = chain
            .header_chain
            .iter_headers()
            .map(|header| header.header)
            .collect::<Vec<Header>>();
        assert_eq!(
            block_iter,
            vec![block_5, new_block_4, block_3, block_2, block_1]
        );
    }

    #[tokio::test]
    async fn test_filters_out_of_order() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 1);
        let scenario = load_scenario();
        let header_batch = scenario.most_work_headers();
        let block_5 = scenario.last_block_header();
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        height_monitor.lock().await.insert(1.into(), 2501);
        assert!(chain.is_synced().await);
        let filter_hashes = scenario.n_most_work_filter_hashes(5);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(CFHeaderChanges::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message();
        for filter in scenario.filters().into_iter().rev() {
            assert!(chain.sync_filter(filter).is_ok())
        }
        assert!(chain.is_filters_synced());
    }

    #[tokio::test]
    async fn test_bad_filter() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 1);
        let scenario = load_scenario();
        let header_batch = scenario.most_work_headers();
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        height_monitor.lock().await.insert(1.into(), 2501);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(CFHeaderChanges::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message();
        let mismatch_filter = scenario.filters().first().unwrap().filter.clone();
        let sync_filter_1 = chain.sync_filter(CFilter {
            filter_type: 0x00,
            block_hash: scenario.last_block_hash(),
            filter: mismatch_filter,
        });
        assert!(sync_filter_1.is_err());
        let good_filter = scenario.filters().last().unwrap().clone();
        let sync_filter_1 = chain.sync_filter(good_filter);
        assert!(sync_filter_1.is_ok());
    }

    #[tokio::test]
    async fn test_has_conflict() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let header_batch = scenario.most_work_headers();
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        height_monitor.lock().await.insert(1.into(), 2501);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        let mut conflict_hashes = scenario.n_most_work_filter_hashes(4);
        conflict_hashes.push(
            scenario
                .stale_chain
                .first()
                .map(|data| data.filter_hash())
                .unwrap(),
        );
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: conflict_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Conflict);
        assert!(chain.request_state.pending_batch.is_none());
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(2.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(3.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
        assert!(chain.is_cf_headers_synced());
    }

    #[tokio::test]
    async fn test_uneven_cf_headers() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let header_batch = scenario.most_work_headers();
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        height_monitor.lock().await.insert(1.into(), 2501);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        // Not enough filter hashes
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(4),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_err());
        // Wrong stop hash
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.most_work_headers()[3].into(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_err());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
    }

    #[tokio::test]
    async fn test_reorg_no_queue() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let mut stale_headers = scenario.n_most_work_headers(3);
        let stale_block_data = scenario.stale_chain.first().unwrap();
        stale_headers.push(stale_block_data.header.0);
        let chain_sync = chain.sync_chain(stale_headers);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        // Reorganize the blocks
        let most_work = scenario.most_work_headers();
        let header_batch = vec![most_work[3], most_work[4]];
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        chain.next_cf_header_message();
        let mut stale_hashes = scenario.n_most_work_filter_hashes(3);
        stale_hashes.push(stale_block_data.filter_hash());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: stale_block_data.header.0.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: stale_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_err());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: scenario.last_block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(2.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
        chain.next_filter_message();
        let filters = scenario.filters();
        let sync_filter_1 = chain.sync_filter(filters[0].clone());
        assert!(sync_filter_1.is_ok());
        let sync_filter_2 = chain.sync_filter(filters[1].clone());
        assert!(sync_filter_2.is_ok());
        let sync_filter_4 = chain.sync_filter(filters[3].clone());
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_reorg_with_queue() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let mut header_batch = scenario.n_most_work_headers(3);
        let stale = scenario
            .stale_chain
            .first()
            .map(|data| data.header.0)
            .unwrap();
        header_batch.push(stale);
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let mut stale_hashes = scenario.n_most_work_filter_hashes(3);
        let stale_hash = scenario
            .stale_chain
            .first()
            .map(|data| data.filter_hash())
            .unwrap();
        stale_hashes.push(stale_hash);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: stale.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: stale_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        // Reorganize the blocks
        let new_chain = scenario.most_work_headers();
        let new_block_4 = new_chain[3];
        let block_5 = new_chain[4];
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        // Request the CF headers again
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(2.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
    }

    #[tokio::test]
    #[ignore = "temporarily broken due to hex decoding"]
    async fn reorg_during_filter_sync() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let new_filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let new_filter_hash_4 = sha256d::Hash::hash(&new_filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let new_filter_hash_4 = FilterHash::from_raw_hash(new_filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
        chain.next_filter_message();
        let sync_filter_1 = chain.sync_filter(CFilter {
            filter_type: 0x00,
            block_hash: block_1.block_hash(),
            filter: filter_1,
        });
        assert!(sync_filter_1.is_ok());
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        height_monitor.lock().await.increment(1.into());
        assert!(chain.is_synced().await);
        // Request the headers again
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("4818ea31ceccf249909aad97f1da4f8ec2ca5738fb56b2f8b443b80fe8f91387")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![new_filter_hash_4, filter_hash_5],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("4818ea31ceccf249909aad97f1da4f8ec2ca5738fb56b2f8b443b80fe8f91387")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![new_filter_hash_4, filter_hash_5],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::Extended);
        let sync_filter_4 = chain.sync_filter(CFilter {
            filter_type: 0x00,
            block_hash: block_4.block_hash(),
            filter: filter_4,
        });
        assert!(sync_filter_4.is_err());
        let sync_filter_4 = chain.sync_filter(CFilter {
            filter_type: 0x00,
            block_hash: new_block_4.block_hash(),
            filter: new_filter_4,
        });
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_inv_no_queue() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let header_batch = scenario.n_most_work_headers(4);
        let block_4 = header_batch
            .last()
            .map(|header| header.block_hash())
            .unwrap();
        let chain_sync = chain.sync_chain(header_batch);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let block_5 = scenario.last_block_header();
        let chain_sync = chain.sync_chain(vec![block_5]);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4,
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(4),
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_err());
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(2.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: scenario.n_most_work_filter_hashes(5),
        };
        let cf_header_sync_res = chain.sync_cf_headers(2.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
    }

    #[tokio::test]
    async fn test_inv_with_queue() {
        let gen = base_block();
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let scenario = load_scenario();
        let first_four = scenario.n_most_work_headers(4);
        let block_4 = first_four.last().copied().unwrap();
        let chain_sync = chain.sync_chain(first_four);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let first_four_filter_hashes = scenario.n_most_work_filter_hashes(4);
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: first_four_filter_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        let last_header = scenario.last_block_header();
        let all_filter_hashes = scenario.n_most_work_filter_hashes(5);
        let chain_sync = chain.sync_chain(vec![last_header]);
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        chain.clear_compact_filter_queue();
        chain.next_cf_header_message();
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: last_header.block_hash(),
            previous_filter_header: scenario.prev_header(),
            filter_hashes: all_filter_hashes,
        };
        let cf_header_sync_res = chain.sync_cf_headers(1.into(), cf_headers);
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), CFHeaderChanges::AddedToQueue);
        assert!(!chain.is_cf_headers_synced());
    }
}
