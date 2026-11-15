use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use bitcoin::BlockHash;
use tokio::{sync::oneshot, time::Instant};

use crate::{error::FetchBlockError, messages::ClientRequest, BlockType, IndexedBlock};

const RETRY_PADDING: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct BlockQueue {
    queue: VecDeque<Request>,
    want: Option<Request>,
    last_req: Instant,
    spam_limit: Duration,
    completed: HashSet<BlockHash>,
}

impl BlockQueue {
    pub(crate) fn new(response_timeout: Duration) -> Self {
        Self {
            queue: VecDeque::new(),
            want: None,
            last_req: Instant::now(),
            spam_limit: response_timeout + RETRY_PADDING,
            completed: HashSet::new(),
        }
    }

    pub(crate) fn add(&mut self, request: impl Into<Request>) {
        let request: Request = request.into();
        self.queue.push_front(request)
    }

    pub(crate) fn pop(&mut self) -> Option<(BlockHash, Option<BlockType>)> {
        match self.want.as_mut() {
            Some(request) => {
                if self.last_req.elapsed() < self.spam_limit {
                    None
                } else {
                    self.last_req = Instant::now();
                    Some((request.hash, request.block_type))
                }
            }
            None => {
                self.last_req = Instant::now();
                let request = self.queue.pop_back();
                let next = request
                    .as_ref()
                    .map(|request| (request.hash, request.block_type));
                self.want = request;
                next
            }
        }
    }

    pub(crate) fn process_block(&mut self, block: &BlockHash) -> ProcessBlockResponse {
        if let Some(request) = self.want.take() {
            if request.hash.eq(block) {
                self.want = None;
                self.completed.insert(*block);
                return ProcessBlockResponse::Accepted {
                    block_recipient: request.recipient,
                };
            // We still need whatever hash is in the queue
            } else if self.completed.contains(block) {
                self.want = Some(request);
                return ProcessBlockResponse::LateResponse;
            } else {
                self.want = Some(request);
                return ProcessBlockResponse::UnknownHash;
            }
        }
        if self.completed.contains(block) {
            return ProcessBlockResponse::LateResponse;
        }
        ProcessBlockResponse::UnknownHash
    }

    #[allow(unused)]
    pub(crate) fn complete(&self) -> bool {
        self.want.is_none() && self.queue.is_empty()
    }

    pub(crate) fn remove(&mut self, hashes: &[BlockHash]) {
        let (reorged, retained): (VecDeque<Request>, VecDeque<Request>) = self
            .queue
            .drain(..)
            .partition(|request| hashes.contains(&request.hash));
        self.queue = retained;
        for request in reorged {
            let _ = request
                .recipient
                .send(Err(FetchBlockError::Reorged(request.hash)));
        }
        if let Some(want) = self.want.take_if(|want| hashes.contains(&want.hash)) {
            let _ = want
                .recipient
                .send(Err(FetchBlockError::Reorged(want.hash)));
        }
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    hash: BlockHash,
    block_type: Option<BlockType>,
    recipient: oneshot::Sender<Result<IndexedBlock, FetchBlockError>>,
}

impl Request {
    fn from_block_request(
        block_request: ClientRequest<
            (BlockHash, Option<BlockType>),
            Result<IndexedBlock, FetchBlockError>,
        >,
    ) -> Self {
        let ((hash, block_type), oneshot) = block_request.into_values();
        Self {
            hash,
            block_type,
            recipient: oneshot,
        }
    }
}

impl From<ClientRequest<(BlockHash, Option<BlockType>), Result<IndexedBlock, FetchBlockError>>>
    for Request
{
    fn from(
        value: ClientRequest<(BlockHash, Option<BlockType>), Result<IndexedBlock, FetchBlockError>>,
    ) -> Self {
        Request::from_block_request(value)
    }
}

#[derive(Debug)]
pub(crate) enum ProcessBlockResponse {
    Accepted {
        block_recipient: oneshot::Sender<Result<IndexedBlock, FetchBlockError>>,
    },
    LateResponse,
    UnknownHash,
}

#[cfg(test)]
mod test {
    use std::str::FromStr;
    use std::time::Duration;

    use super::*;

    fn three_block_hashes() -> [BlockHash; 3] {
        let hash_1 =
            BlockHash::from_str("0000007a93b953158a12aef32eb9cc4366eb1eea5892fb04afbeec421c29319d")
                .unwrap();
        let hash_2 =
            BlockHash::from_str("0000009e41d363546c5126c045bdef80e863324ac87f2bec88927a53662f6c0b")
                .unwrap();
        let hash_3 =
            BlockHash::from_str("000000254633c01d43534d80981c3d1e0f4f3541cce2af68084e7631832d2572")
                .unwrap();
        [hash_1, hash_2, hash_3]
    }

    trait DummyRequestExt {
        fn dummy_request(&self) -> Request;
    }

    impl DummyRequestExt for BlockHash {
        fn dummy_request(&self) -> Request {
            let (tx, _rx) = oneshot::channel();
            let client_request = ClientRequest::new((*self, None), tx);
            Request::from_block_request(client_request)
        }
    }

    #[test]
    fn test_block_queue() {
        let [hash_1, hash_2, hash_3] = three_block_hashes();
        let mut queue = BlockQueue::new(Duration::from_secs(5));
        queue.add(hash_1.dummy_request());
        queue.add(hash_2.dummy_request());
        queue.add(hash_3.dummy_request());
        queue.add(hash_1.dummy_request());
        assert_eq!(queue.queue.len(), 4);
        assert_eq!(queue.pop(), Some((hash_1, None)));
        assert_eq!(queue.pop(), None);
        assert_eq!(
            queue.want.as_ref().map(|request| request.hash),
            Some(hash_1)
        );
        queue.process_block(&hash_1);
        assert_eq!(queue.want.as_ref().map(|request| request.hash), None);
        assert_eq!(queue.pop(), Some((hash_2, None)));
        assert_eq!(
            queue.want.as_ref().map(|request| request.hash),
            Some(hash_2)
        );
        queue.process_block(&hash_2);
        assert_eq!(queue.pop(), Some((hash_3, None)));
        assert!(!queue.complete());
        assert_eq!(queue.pop(), None);
        assert!(!queue.complete());
        queue.process_block(&hash_2);
        assert!(!queue.complete());
        queue.process_block(&hash_3);
        assert!(!queue.complete());
        assert_eq!(queue.pop(), Some((hash_1, None)));
        queue.process_block(&hash_1);
        assert!(queue.complete());
    }

    #[tokio::test(start_paused = true)]
    async fn test_laggy_peer() {
        let [hash_1, hash_2, hash_3] = three_block_hashes();
        let mut queue = BlockQueue::new(Duration::from_secs(5));
        queue.add(hash_1.dummy_request());
        queue.add(hash_2.dummy_request());
        queue.add(hash_3.dummy_request());
        assert_eq!(queue.queue.len(), 3);
        assert_eq!(queue.pop(), Some((hash_1, None)));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(queue.pop(), Some((hash_1, None)));
        assert_eq!(
            queue.want.as_ref().map(|request| request.hash),
            Some(hash_1)
        );
        queue.process_block(&hash_1);
        assert_eq!(queue.want.as_ref().map(|request| request.hash), None);
        assert_eq!(queue.pop(), Some((hash_2, None)));
        assert_eq!(
            queue.want.as_ref().map(|request| request.hash),
            Some(hash_2)
        );
        queue.process_block(&hash_2);
        assert_eq!(queue.pop(), Some((hash_3, None)));
        assert!(!queue.complete());
        assert_eq!(queue.pop(), None);
        assert!(!queue.complete());
        let response = queue.process_block(&hash_2);
        assert!(matches!(response, ProcessBlockResponse::LateResponse));
        assert!(!queue.complete());
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(queue.pop(), Some((hash_3, None)));
        assert!(!queue.complete());
        queue.process_block(&hash_3);
        assert!(queue.complete());
        assert_eq!(queue.pop(), None);
        let response = queue.process_block(&hash_3);
        assert!(matches!(response, ProcessBlockResponse::LateResponse));
    }

    #[test]
    fn test_blocks_removed() {
        let [hash_1, hash_2, hash_3] = three_block_hashes();
        let mut queue = BlockQueue::new(Duration::from_secs(5));
        let (tx_1, mut rx_1) = oneshot::channel();
        let (tx_2, mut rx_2) = oneshot::channel();
        let (tx_3, mut rx_3) = oneshot::channel();
        let (tx_4, mut rx_4) = oneshot::channel();
        queue.add(ClientRequest::new((hash_1, None), tx_1));
        queue.add(ClientRequest::new((hash_2, None), tx_2));
        queue.add(ClientRequest::new((hash_3, None), tx_3));
        queue.add(ClientRequest::new((hash_1, None), tx_4));
        assert_eq!(queue.queue.len(), 4);
        assert_eq!(queue.pop(), Some((hash_1, None)));
        assert_eq!(
            queue.want.as_ref().map(|request| request.hash),
            Some(hash_1)
        );
        queue.remove(&[hash_1]);
        assert_eq!(queue.want.as_ref().map(|request| request.hash), None);
        assert!(
            matches!(rx_1.try_recv(), Ok(Err(FetchBlockError::Reorged(hash))) if hash == hash_1)
        );
        assert!(
            matches!(rx_4.try_recv(), Ok(Err(FetchBlockError::Reorged(hash))) if hash == hash_1)
        );
        queue.remove(&[hash_2]);
        assert!(
            matches!(rx_2.try_recv(), Ok(Err(FetchBlockError::Reorged(hash))) if hash == hash_2)
        );
        assert_eq!(queue.queue.len(), 1);
        assert_eq!(queue.pop(), Some((hash_3, None)));
        assert!(rx_3.try_recv().is_err());
    }
}
