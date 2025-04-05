extern crate alloc;
use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    sync::Arc,
};

use bitcoin::{
    block::Header,
    p2p::message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
    Block, BlockHash, Network, ScriptBuf, TxOut,
};
use tokio::sync::Mutex;

use super::{
    block_queue::BlockQueue,
    checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
    error::{BlockScanError, HeaderSyncError},
    graph::{AcceptHeaderChanges, BlockTree, HeaderRejection, Tip},
    HeightMonitor,
};
#[cfg(feature = "filter-control")]
use crate::error::FetchBlockError;
#[cfg(feature = "filter-control")]
use crate::messages::BlockRequest;
#[cfg(feature = "filter-control")]
use crate::IndexedFilter;
use crate::{
    chain::header_batch::HeadersBatch,
    db::{traits::HeaderStore, BlockHeaderChanges},
    dialog::Dialog,
    error::HeaderPersistenceError,
    filters::{
        cfheader_batch::CFHeaderBatch,
        cfheader_chain::{AppendAttempt, CFHeaderChain, QueuedCFHeader},
        error::{CFHeaderSyncError, CFilterSyncError},
        Filter, CF_HEADER_BATCH_SIZE, FILTER_BATCH_SIZE,
    },
    messages::{Event, Warning},
    IndexedBlock,
};

const REORG_LOOKBACK: u32 = 7;
const FILTER_BASIC: u8 = 0x00;

#[derive(Debug)]
pub(crate) struct Chain<H: HeaderStore> {
    pub(crate) header_chain: BlockTree,
    cf_header_chain: CFHeaderChain,
    last_stop_hash_request: Option<BlockHash>,
    checkpoints: HeaderCheckpoints,
    network: Network,
    db: Arc<Mutex<H>>,
    heights: Arc<Mutex<HeightMonitor>>,
    scripts: HashSet<ScriptBuf>,
    block_queue: BlockQueue,
    dialog: Arc<Dialog>,
}

impl<H: HeaderStore> Chain<H> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        network: Network,
        scripts: HashSet<ScriptBuf>,
        anchor: HeaderCheckpoint,
        checkpoints: HeaderCheckpoints,
        dialog: Arc<Dialog>,
        height_monitor: Arc<Mutex<HeightMonitor>>,
        db: H,
        quorum_required: usize,
    ) -> Self {
        let header_chain = BlockTree::new(anchor, network);
        let cf_header_chain = CFHeaderChain::new(anchor, quorum_required);
        Chain {
            header_chain,
            checkpoints,
            network,
            db: Arc::new(Mutex::new(db)),
            cf_header_chain,
            last_stop_hash_request: None,
            heights: height_monitor,
            scripts,
            block_queue: BlockQueue::new(),
            dialog,
        }
    }

    // This header chain contains a block hash, potentially checking the disk
    pub(crate) async fn height_of_hash(&self, blockhash: BlockHash) -> Option<u32> {
        match self.header_chain.height_of_hash(blockhash) {
            Some(height) => Some(height),
            None => {
                let mut lock = self.db.lock().await;
                lock.height_of(&blockhash).await.unwrap_or(None)
            }
        }
    }

    // This header chain contains a block hash in memory
    pub(crate) fn cached_header_at_height(&self, height: u32) -> Option<Header> {
        self.header_chain.header_at_height(height)
    }

    // Fetch a header from the cache or disk.
    pub(crate) async fn fetch_header(
        &mut self,
        height: u32,
    ) -> Result<Option<Header>, HeaderPersistenceError<H::Error>> {
        match self.header_chain.header_at_height(height) {
            Some(header) => Ok(Some(header)),
            None => {
                let mut db = self.db.lock().await;
                let header_opt = db.header_at(height).await;
                if header_opt.is_err() {
                    self.dialog
                        .send_warning(Warning::FailedPersistence {
                            warning: format!(
                                "Unexpected error fetching a header from the header store at height {height}"
                            ),
                        });
                }
                header_opt.map_err(HeaderPersistenceError::Database)
            }
        }
    }

    // The hash at the given height, potentially checking on disk
    pub(crate) async fn blockhash_at_height(&self, height: u32) -> Option<BlockHash> {
        match self
            .cached_header_at_height(height)
            .map(|header| header.block_hash())
        {
            Some(hash) => Some(hash),
            None => {
                let mut lock = self.db.lock().await;
                lock.hash_at(height).await.unwrap_or(None)
            }
        }
    }

    // Have we hit the known checkpoints
    pub(crate) fn checkpoints_complete(&self) -> bool {
        self.checkpoints.is_exhausted()
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

    // The "locators" are the headers we inform our peers we know about
    pub(crate) async fn locators(&mut self) -> Vec<BlockHash> {
        // If a peer is sending us a fork at this point they are faulty.
        if !self.checkpoints_complete() {
            vec![self.header_chain.tip_hash()]
        } else {
            // We should try to catch any reorgs if we are on a fresh start.
            // The database may have a header that is useful to the remote node
            // that is not currently in memory.
            if self.header_chain.internally_cached_headers() < REORG_LOOKBACK as usize {
                let older_locator = self.header_chain.height().saturating_sub(REORG_LOOKBACK);
                let mut db_lock = self.db.lock().await;
                let hash = db_lock.hash_at(older_locator).await;
                if let Ok(Some(locator)) = hash {
                    vec![self.header_chain.tip_hash(), locator]
                } else {
                    // We couldn't find a header deep enough to send over. Just proceed as usual
                    self.header_chain.locators()
                }
            } else {
                // We have enough headers in memory to catch a reorg.
                self.header_chain.locators()
            }
        }
    }

    // Write the chain to disk
    pub(crate) async fn write_changes(&mut self, changes: BlockHeaderChanges) {
        if let Err(e) = self.db.lock().await.write(changes).await {
            self.dialog.send_warning(Warning::FailedPersistence {
                warning: format!("Could not save headers to disk: {e}"),
            });
        }
    }

    // Load in the headers
    pub(crate) async fn load_headers(&mut self) -> Result<(), HeaderPersistenceError<H::Error>> {
        let loaded_headers = self
            .db
            .lock()
            .await
            .load(self.header_chain.height() + 1..)
            .await
            .map_err(HeaderPersistenceError::Database)?;
        for (height, header) in loaded_headers {
            let apply_header_changes = self.header_chain.accept_header(header);
            match apply_header_changes {
                AcceptHeaderChanges::Accepted { connected_at } => {
                    if height.ne(&connected_at.height) {
                        self.dialog.send_warning(Warning::CorruptedHeaders);
                        return Err(HeaderPersistenceError::HeadersDoNotLink);
                    }
                    if let Some(checkpoint) = self.checkpoints.next() {
                        if connected_at.header.block_hash().eq(&checkpoint.hash) {
                            self.checkpoints.advance()
                        }
                    }
                }
                AcceptHeaderChanges::Rejected(reject_reason) => match reject_reason {
                    HeaderRejection::UnknownPrevHash(_) => {
                        return Err(HeaderPersistenceError::CannotLocateHistory);
                    }
                    HeaderRejection::InvalidPow { expected, got } => {
                        crate::log!(
                            self.dialog,
                            format!(
                                "Unexpected invalid proof of work when importing a block header. expected {}, got: {}",
                                expected.to_consensus(),
                                got.to_consensus()
                            )
                        );
                    }
                },
                _ => (),
            }
        }
        Ok(())
    }

    // Sync the chain with headers from a peer, adjusting to reorgs if needed
    pub(crate) async fn sync_chain(&mut self, message: Vec<Header>) -> Result<(), HeaderSyncError> {
        let header_batch = HeadersBatch::new(message).map_err(|_| HeaderSyncError::EmptyMessage)?;
        // If our chain already has the last header in the message there is no new information
        if self.header_chain.contains(header_batch.last().block_hash()) {
            return Ok(());
        }
        // We check first if the peer is sending us nonsense
        self.sanity_check(&header_batch)?;
        let next_checkpoint = self.checkpoints.next().copied();
        for header in header_batch.into_iter() {
            let changes = self.header_chain.accept_header(header);
            match changes {
                AcceptHeaderChanges::Accepted { connected_at } => {
                    crate::log!(
                        self.dialog,
                        format!(
                            "Chain updated {} -> {}",
                            connected_at.height,
                            connected_at.header.block_hash()
                        )
                    );
                    self.write_changes(BlockHeaderChanges::Connected(connected_at))
                        .await;
                    if let Some(checkpoint) = next_checkpoint {
                        if connected_at.height.eq(&checkpoint.height) {
                            if connected_at.header.block_hash().eq(&checkpoint.hash) {
                                crate::log!(
                                    self.dialog,
                                    format!("Found checkpoint, height: {}", checkpoint.height)
                                );
                                self.checkpoints.advance();
                            } else {
                                self.dialog
                    .send_warning(
                        Warning::UnexpectedSyncError { warning: "Unmatched checkpoint sent by a peer. Restarting header sync with new peers.".into() }
                    );
                                return Err(HeaderSyncError::InvalidCheckpoint);
                            }
                        }
                    }
                }
                AcceptHeaderChanges::Duplicate => (),
                AcceptHeaderChanges::ExtendedFork { connected_at } => match next_checkpoint {
                    Some(_checkpoint_expected) => {
                        crate::log!(self.dialog, "Detected fork before known checkpoint");
                        self.dialog.send_warning(Warning::UnexpectedSyncError {
                            warning: "Pre-checkpoint fork".into(),
                        });
                    }
                    None => {
                        crate::log!(
                            self.dialog,
                            format!("Fork created or extended {}", connected_at.height)
                        )
                    }
                },
                AcceptHeaderChanges::Reorganization {
                    accepted,
                    disconnected,
                } => {
                    crate::log!(self.dialog, "Valid reorganization found");
                    self.clear_compact_filter_queue();
                    let removed_hashes: Vec<BlockHash> = disconnected
                        .iter()
                        .map(|index| index.header.block_hash())
                        .collect();
                    self.cf_header_chain.remove(&removed_hashes);
                    self.block_queue.remove(&removed_hashes);
                    self.write_changes(BlockHeaderChanges::Reorganized {
                        accepted,
                        reorganized: disconnected.clone(),
                    })
                    .await;
                    let disconnected_event =
                        Event::BlocksDisconnected(disconnected.into_iter().rev().collect());
                    self.dialog.send_event(disconnected_event);
                }
                AcceptHeaderChanges::Rejected(rejected_header) => match rejected_header {
                    HeaderRejection::InvalidPow {
                        expected: _,
                        got: _,
                    } => return Err(HeaderSyncError::InvalidBits),
                    HeaderRejection::UnknownPrevHash(hash) => {
                        let mut db = self.db.lock().await;
                        let header_res = db.height_of(&hash).await.ok().flatten();
                        match header_res {
                            Some(height) => {
                                let tip = Tip::from_checkpoint(height, hash);
                                self.header_chain = BlockTree::new(tip, self.network);
                                self.cf_header_chain = CFHeaderChain::new(
                                    HeaderCheckpoint::new(height, hash),
                                    self.cf_header_chain.quorum_required(),
                                );
                                drop(db);
                                if let Err(e) = self.load_headers().await {
                                    crate::log!(self.dialog,
                                        "Failure when attempting to fetch previous headers while syncing"
                                    );
                                    self.dialog.send_warning(Warning::FailedPersistence {
                                        warning: format!("Persistence failure: {e}"),
                                    });
                                }
                            }
                            None => return Err(HeaderSyncError::FloatingHeaders),
                        }
                    }
                },
            }
        }
        Ok(())
    }

    // These are invariants in all batches of headers we receive
    fn sanity_check(&mut self, header_batch: &HeadersBatch) -> Result<(), HeaderSyncError> {
        // All the headers connect with each other and is the difficulty adjustment not absurd
        if !header_batch.connected() {
            return Err(HeaderSyncError::HeadersNotConnected);
        }

        // All headers pass their own proof of work and the network minimum
        if !header_batch.individually_valid_pow() {
            return Err(HeaderSyncError::InvalidHeaderWork);
        }

        if !header_batch.bits_adhere_transition(self.network) {
            return Err(HeaderSyncError::InvalidBits);
        }

        Ok(())
    }

    // Sync the compact filter headers, possibly encountering conflicts
    pub(crate) async fn sync_cf_headers(
        &mut self,
        _peer_id: u32,
        cf_headers: CFHeaders,
    ) -> Result<AppendAttempt, CFHeaderSyncError> {
        let mut batch: CFHeaderBatch = cf_headers.into();
        let peer_max = self.heights.lock().await.max();
        self.dialog
            .chain_update(
                self.header_chain.height(),
                self.cf_header_chain.height(),
                self.header_chain
                    .iter_filters_checked()
                    .filter(|checked| *checked)
                    .count() as u32,
                peer_max.unwrap_or(self.header_chain.height()),
            )
            .await;
        match batch.last_header() {
            Some(batch_last) => {
                if let Some(prev_header) = self.cf_header_chain.prev_header() {
                    // A new block was mined and we ended up asking for this batch twice,
                    // or the quorum required is less than our connected peers.
                    if batch_last.eq(&prev_header) {
                        return Ok(AppendAttempt::AddedToQueue);
                    }
                }
            }
            None => return Err(CFHeaderSyncError::EmptyMessage),
        }
        // Check for any obvious faults
        self.audit_cf_headers(&batch).await?;
        // We already have a message like this. Verify they are the same
        match self.cf_header_chain.merged_queue.take() {
            Some(queue) => Ok(self.cf_header_chain.verify(&mut batch, queue)),
            None => {
                let queue = self.construct_cf_header_queue(&mut batch).await?;
                Ok(self.cf_header_chain.set_queue(queue))
            }
        }
    }

    // We need to associate the block hash with the incoming filter hashes
    async fn construct_cf_header_queue(
        &self,
        batch: &mut CFHeaderBatch,
    ) -> Result<Vec<QueuedCFHeader>, CFHeaderSyncError> {
        let mut queue = Vec::new();
        let ref_height = self.cf_header_chain.height();
        for (index, (filter_header, filter_hash)) in batch.take_inner().into_iter().enumerate() {
            let block_hash = self
                // This call may or may not retrieve the hash from disk
                .blockhash_at_height(ref_height + index as u32 + 1)
                .await
                .ok_or(CFHeaderSyncError::HeaderChainIndexOverflow)?;
            queue.push(QueuedCFHeader::new(block_hash, filter_header, filter_hash))
        }
        Ok(queue)
    }

    // Audit the validity of a batch of compact filter headers
    async fn audit_cf_headers(&mut self, batch: &CFHeaderBatch) -> Result<(), CFHeaderSyncError> {
        // Does the filter header line up with our current chain of filter headers
        if let Some(prev_header) = self.cf_header_chain.prev_header() {
            if batch.prev_header().ne(&prev_header) {
                return Err(CFHeaderSyncError::PrevHeaderMismatch);
            }
        }
        // Did we request up to this stop hash. We should have caught if this was a repeated message.
        let prev_stophash = self
            .cf_header_chain
            .last_stop_hash_request()
            .ok_or(CFHeaderSyncError::UnexpectedCFHeaderMessage)?;
        if prev_stophash.ne(batch.stop_hash()) {
            return Err(CFHeaderSyncError::StopHashMismatch);
        }
        // Did they send us the right amount of headers
        let stop_hash =
            // This call may or may not retrieve the hash from disk
            self.blockhash_at_height(self.cf_header_chain.height() + batch.len() as u32)
            .await
            .ok_or(CFHeaderSyncError::HeaderChainIndexOverflow)?;
        if stop_hash.ne(batch.stop_hash()) {
            return Err(CFHeaderSyncError::StopHashMismatch);
        }
        Ok(())
    }

    // We need to make this public for new peers that connect to us throughout syncing the filter headers
    pub(crate) async fn next_cf_header_message(&mut self) -> GetCFHeaders {
        let stop_hash_index = self.cf_header_chain.height() + CF_HEADER_BATCH_SIZE + 1;
        let stop_hash = self
            .blockhash_at_height(stop_hash_index)
            .await
            .unwrap_or(self.header_chain.tip_hash());
        self.cf_header_chain.set_last_stop_hash(stop_hash);
        GetCFHeaders {
            filter_type: FILTER_BASIC,
            start_height: self.cf_header_chain.height() + 1,
            stop_hash,
        }
    }

    // Are the compact filter headers caught up to the header chain
    pub(crate) fn is_cf_headers_synced(&self) -> bool {
        self.header_chain
            .height()
            .le(&self.cf_header_chain.height())
    }

    // Handle a new filter
    pub(crate) async fn sync_filter(
        &mut self,
        filter_message: CFilter,
    ) -> Result<Option<GetCFilters>, CFilterSyncError> {
        if self.is_filters_synced() {
            return Ok(None);
        }
        let mut filter = Filter::new(filter_message.filter, filter_message.block_hash);
        let expected_filter_hash = self.cf_header_chain.hash_at(&filter_message.block_hash);
        // Disallow any filter that we do not have a block hash for
        match expected_filter_hash {
            Some(ref_hash) => {
                if filter.filter_hash().ne(ref_hash) {
                    return Err(CFilterSyncError::MisalignedFilterHash);
                }
            }
            None => {
                return Err(CFilterSyncError::UnknownFilterHash);
            }
        }

        #[cfg(feature = "filter-control")]
        {
            let height = self
                .height_of_hash(filter_message.block_hash)
                .await
                .ok_or(CFilterSyncError::UnknownFilterHash)?;
            let indexed_filter = IndexedFilter::new(height, filter);
            self.dialog.send_event(Event::IndexedFilter(indexed_filter));
        }

        #[cfg(not(feature = "filter-control"))]
        if !self.block_queue.contains(&filter_message.block_hash)
            && filter
                .contains_any(self.scripts.iter())
                .map_err(CFilterSyncError::Filter)?
        {
            // Add to the block queue
            self.block_queue.add(filter_message.block_hash);
            crate::log!(
                self.dialog,
                format!("Found script at block: {}", filter_message.block_hash)
            );
        }
        self.header_chain.filter_checked(filter_message.block_hash);
        let stop_hash = self
            .last_stop_hash_request
            .ok_or(CFilterSyncError::UnrequestedStophash)?;
        if filter_message.block_hash.eq(&stop_hash) {
            if !self.is_filters_synced() {
                Ok(Some(self.next_filter_message().await))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    // Next filter message, if there is one
    pub(crate) async fn next_filter_message(&mut self) -> GetCFilters {
        let mut last_unchecked_hash = self.header_chain.height();
        for block_data in self.header_chain.iter_data() {
            if block_data.filter_checked {
                break;
            }
            last_unchecked_hash = block_data.height.to_u32();
        }
        let stop_hash_index = last_unchecked_hash + FILTER_BATCH_SIZE;
        let stop_hash = self
            .blockhash_at_height(stop_hash_index)
            .await
            .unwrap_or(self.header_chain.tip_hash());
        let peer_max = self.heights.lock().await.max();
        self.dialog
            .chain_update(
                self.header_chain.height(),
                self.cf_header_chain.height(),
                self.header_chain
                    .iter_filters_checked()
                    .filter(|checked| *checked)
                    .count() as u32,
                peer_max.unwrap_or(self.header_chain.height()),
            )
            .await;
        self.last_stop_hash_request = Some(stop_hash);
        GetCFilters {
            filter_type: FILTER_BASIC,
            start_height: last_unchecked_hash,
            stop_hash,
        }
    }

    // Are we synced with filters
    pub(crate) fn is_filters_synced(&self) -> bool {
        self.header_chain.all_filters_checked()
    }

    // Pop a block from the queue of interesting blocks
    pub(crate) fn next_block(&mut self) -> Option<BlockHash> {
        self.block_queue.pop()
    }

    // Are there any blocks left in the queue
    pub(crate) fn block_queue_empty(&self) -> bool {
        self.block_queue.complete()
    }

    // Make sure we have this hash in our chain, check the merkle root, and pass the block
    pub(crate) async fn check_send_block(&mut self, block: Block) -> Result<(), BlockScanError> {
        let block_hash = block.block_hash();
        if !self.block_queue.need(&block_hash) {
            return Ok(());
        }
        let height = self
            .height_of_hash(block_hash)
            .await
            .ok_or(BlockScanError::NoBlockHash)?;
        if !block.check_merkle_root() {
            return Err(BlockScanError::InvalidMerkleRoot);
        }
        let sender = self.block_queue.receive(&block_hash);
        match sender {
            Some(sender) => {
                let send_result = sender.send(Ok(IndexedBlock::new(height, block)));
                if send_result.is_err() {
                    self.dialog.send_warning(Warning::ChannelDropped)
                };
            }
            None => {
                self.dialog
                    .send_event(Event::Block(IndexedBlock::new(height, block)));
            }
        }
        Ok(())
    }

    // Should we care about this block
    #[allow(dead_code)]
    fn scan_outputs(&mut self, inputs: &[TxOut]) -> bool {
        inputs
            .iter()
            .any(|out| self.scripts.contains(&out.script_pubkey))
    }

    // Add a script to our list
    pub(crate) fn put_script(&mut self, script: ScriptBuf) {
        self.scripts.insert(script);
    }

    // Explicitly request a block
    #[cfg(feature = "filter-control")]
    pub(crate) async fn get_block(&mut self, request: BlockRequest) {
        let height_opt = self.height_of_hash(request.hash).await;
        if height_opt.is_none() {
            let err_reponse = request.oneshot.send(Err(FetchBlockError::UnknownHash));
            if err_reponse.is_err() {
                self.dialog.send_warning(Warning::ChannelDropped);
            }
        } else {
            crate::log!(
                self.dialog,
                format!("Adding block {} to queue", request.hash)
            );
            self.block_queue.add(request)
        }
    }

    pub(crate) async fn fetch_header_range(
        &self,
        range: Range<u32>,
    ) -> Result<BTreeMap<u32, Header>, HeaderPersistenceError<H::Error>> {
        let mut db = self.db.lock().await;
        let range_opt = db.load(range).await;
        if range_opt.is_err() {
            self.dialog.send_warning(Warning::FailedPersistence {
                warning: "Unexpected error fetching a range of headers from the header store"
                    .to_string(),
            });
        }
        range_opt.map_err(HeaderPersistenceError::Database)
    }

    // Reset the compact filter queue because we received a new block
    pub(crate) fn clear_compact_filter_queue(&mut self) {
        self.cf_header_chain.clear_queue();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::{collections::HashSet, str::FromStr};

    use bitcoin::hashes::sha256d;
    use bitcoin::hashes::Hash;
    use bitcoin::{
        block::Header,
        consensus::deserialize,
        p2p::message_filter::{CFHeaders, CFilter},
        BlockHash, FilterHash, FilterHeader,
    };
    use tokio::sync::Mutex;

    use crate::{
        chain::checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
        filters::cfheader_chain::AppendAttempt,
        {
            dialog::Dialog,
            messages::{Event, Log, Warning},
        },
    };

    use super::{Chain, HeightMonitor};

    fn new_regtest(
        anchor: HeaderCheckpoint,
        height_monitor: Arc<Mutex<HeightMonitor>>,
        peers: usize,
    ) -> Chain<()> {
        let (log_tx, _) = tokio::sync::mpsc::channel::<Log>(1);
        let (warn_tx, _) = tokio::sync::mpsc::unbounded_channel::<Warning>();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let mut checkpoints = HeaderCheckpoints::new(&bitcoin::Network::Regtest);
        checkpoints.prune_up_to(anchor);
        Chain::new(
            bitcoin::Network::Regtest,
            HashSet::new(),
            anchor,
            checkpoints,
            Arc::new(Dialog::new(
                crate::LogLevel::Debug,
                log_tx,
                warn_tx,
                event_tx,
            )),
            height_monitor,
            (),
            peers,
        )
    }

    #[tokio::test]
    async fn test_fork_includes_old_vals() {
        let gen = HeaderCheckpoint::new(
            0,
            BlockHash::from_str("0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor, 1);
        let block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f047eb4d0fe76345e307d0e020a079cedfa37101ee7ac84575cf829a611b0f84bc4805e66ffff7f2001000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("00000020299e41732deb76d869fcdb5f72518d3784e99482f572afb73068d52134f1f75e1f20f5da8d18661d0f13aa3db8fff0f53598f7d61f56988a6d66573394b2c6ffc5805e66ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea3471f1dbedc283ce6a43a87ed3c8e6326dae8d3dbacce1b2daba08e508054ffdb697815e66ffff7f2001000000").unwrap()).unwrap();
        let batch_1 = vec![block_1, block_2, block_3];
        let new_block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea349c6240c5d0521966771808950f796c9c04088bc9551a828b64f1cf06831705dfbc835e66ffff7f2000000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("00000020d2a1c6ba2be393f405fe2f4574565f9ee38ac68d264872fcd82b030970d0232ce882eb47c3dd138587120f1ad97dd0e73d1e30b79559ad516cb131f83dcb87e9bc835e66ffff7f2002000000").unwrap()).unwrap();
        let batch_2 = vec![block_1, block_2, new_block_3, block_4];
        let chain_sync = chain.sync_chain(batch_1).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 3);
        let mut index = 1;
        for block in vec![block_1, block_2, block_3] {
            assert_eq!(
                chain.header_chain.block_hash_at_height(index).unwrap(),
                block.into()
            );
            index += 1;
        }
        let chain_sync = chain.sync_chain(batch_2).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 4);
        let mut index = 1;
        for block in vec![block_1, block_2, new_block_3, block_4] {
            assert_eq!(
                chain.header_chain.block_hash_at_height(index).unwrap(),
                block.into()
            );
            index += 1;
        }
    }

    #[tokio::test]
    async fn test_filters_out_of_order() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 1);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        let sync_filter_3 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_3.block_hash(),
                filter: filter_3,
            })
            .await;
        assert!(sync_filter_3.is_ok());
        let sync_filter_2 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_2.block_hash(),
                filter: filter_2,
            })
            .await;
        assert!(sync_filter_2.is_ok());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
        assert!(chain.is_filters_synced());
    }

    #[tokio::test]
    async fn test_bad_filter() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 1);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_2,
            })
            .await;
        assert!(sync_filter_1.is_err());
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
    }

    #[tokio::test]
    async fn test_bad_blockhash() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 1);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            // Wrong block hash
            stop_hash: block_3.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        // Try the request again
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_2.block_hash(),
                filter: filter_1.clone(),
            })
            .await;
        assert!(sync_filter_1.is_err());
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
    }

    #[tokio::test]
    async fn test_has_conflict() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_3],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(
            cf_header_sync_res.unwrap(),
            AppendAttempt::Conflict(block_4.block_hash())
        );
        assert!(!chain.cf_header_chain.has_queue());
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
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
        let cf_header_sync_res = chain.sync_cf_headers(3, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        assert!(chain.is_cf_headers_synced());
    }

    #[tokio::test]
    async fn test_uneven_cf_headers() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        // Not enough filter hashes
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        // Wrong stop hash
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_3.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
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
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
    }

    #[tokio::test]
    async fn test_reorg_no_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
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
        chain.next_cf_header_message().await;
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_err());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: new_block_4.block_hash(),
                filter: new_filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_reorg_with_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
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
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        // Request the CF headers again
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
    }

    #[tokio::test]
    async fn reorg_during_filter_sync() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
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
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
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
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        height_monitor.lock().await.increment(1.into());
        assert!(chain.is_synced().await);
        // Request the headers again
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
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
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_err());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: new_block_4.block_hash(),
                filter: new_filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_inv_no_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        let chain_sync = chain.sync_chain(vec![block_5]).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        assert!(chain.is_synced().await);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
    }

    #[tokio::test]
    async fn test_inv_with_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let height_monitor = Arc::new(Mutex::new(HeightMonitor::new()));
        let mut chain = new_regtest(gen, height_monitor.clone(), 2);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2500);
        height_monitor.lock().await.insert(1.into(), 2500);
        assert!(chain.is_synced().await);
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
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
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let chain_sync = chain.sync_chain(vec![block_5]).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.header_chain.height(), 2501);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        assert_eq!(chain.cf_header_chain.height(), 2500);
    }
}
