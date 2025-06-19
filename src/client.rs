#[cfg(not(feature = "filter-control"))]
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::{block::Header, BlockHash, FeeRate};
use std::{collections::BTreeMap, ops::Range, time::Duration};
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::{Event, Info, TrustedPeer, TxBroadcast, Warning};

use super::{error::FetchBlockError, messages::BlockRequest, IndexedBlock};
use super::{
    error::{ClientError, FetchFeeRateError, FetchHeaderError},
    messages::{BatchHeaderRequest, ClientMessage, HeaderRequest},
};

/// A [`Client`] allows for communication with a running node.
#[derive(Debug)]
pub struct Client {
    /// Send events to a node, such as broadcasting a transaction.
    pub requester: Requester,
    /// Receive log/debug messages from a node.
    pub log_rx: mpsc::Receiver<String>,
    /// Receive informational messages from the node.
    pub info_rx: mpsc::Receiver<Info>,
    /// Receive warning messages from a node.
    pub warn_rx: mpsc::UnboundedReceiver<Warning>,
    /// Receive [`Event`] from a node to act on.
    pub event_rx: mpsc::UnboundedReceiver<Event>,
}

impl Client {
    pub(crate) fn new(
        log_rx: mpsc::Receiver<String>,
        info_rx: mpsc::Receiver<Info>,
        warn_rx: mpsc::UnboundedReceiver<Warning>,
        event_rx: mpsc::UnboundedReceiver<Event>,
        ntx: UnboundedSender<ClientMessage>,
    ) -> Self {
        Self {
            requester: Requester::new(ntx),
            log_rx,
            info_rx,
            warn_rx,
            event_rx,
        }
    }
}

/// Send messages to a node that is running so the node may complete a task.
#[derive(Debug, Clone)]
pub struct Requester {
    ntx: UnboundedSender<ClientMessage>,
}

impl Requester {
    fn new(ntx: UnboundedSender<ClientMessage>) -> Self {
        Self { ntx }
    }

    /// Tell the node to shut down.
    ///
    /// # Errors
    ///
    /// If the node has already stopped running.
    pub fn shutdown(&self) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::Shutdown)
            .map_err(|_| ClientError::SendError)
    }

    /// Broadcast a new transaction to the network.
    ///
    /// # Note
    ///
    /// When broadcasting a one-parent one-child (TRUC) package,
    /// broadcast the child first, followed by the parent.
    ///
    /// Package relay is under-development at the time of writing.
    ///
    /// For more information, see BIP-431 and BIP-331.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn broadcast_tx(&self, tx: TxBroadcast) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::Broadcast(tx))
            .map_err(|_| ClientError::SendError)
    }

    /// Broadcast a new transaction to the network to a random peer.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn broadcast_random(&self, tx: Transaction) -> Result<(), ClientError> {
        let tx_broadcast = TxBroadcast::random_broadcast(tx);
        self.ntx
            .send(ClientMessage::Broadcast(tx_broadcast))
            .map_err(|_| ClientError::SendError)
    }

    /// A connection has a minimum transaction fee requirement to enter its mempool. For proper transaction propagation,
    /// transactions should have a fee rate at least as high as the maximum fee filter received.
    /// This method returns the maximum fee rate requirement of all connected peers.
    ///
    /// For more information, refer to BIP133
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn broadcast_min_feerate(&self) -> Result<FeeRate, FetchFeeRateError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<FeeRate>();
        self.ntx
            .send(ClientMessage::GetBroadcastMinFeeRate(tx))
            .map_err(|_| FetchFeeRateError::SendError)?;
        rx.await.map_err(|_| FetchFeeRateError::RecvError)
    }

    /// Add more Bitcoin [`ScriptBuf`] to watch for. Does not rescan the filters.
    /// If the script was already present in the node's collection, no change will occur.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    #[cfg(not(feature = "filter-control"))]
    pub fn add_script(&self, script: impl Into<ScriptBuf>) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::AddScript(script.into()))
            .map_err(|_| ClientError::SendError)
    }

    /// Get a header at the specified height, if it exists.
    ///
    /// # Note
    ///
    /// The height of the chain is the canonical index of the header in the chain.
    /// For example, the genesis block is at a height of zero.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn get_header(&self, height: u32) -> Result<Header, FetchHeaderError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Header, FetchHeaderError>>();
        let message = HeaderRequest::new(tx, height);
        self.ntx
            .send(ClientMessage::GetHeader(message))
            .map_err(|_| FetchHeaderError::SendError)?;
        rx.await.map_err(|_| FetchHeaderError::RecvError)?
    }

    /// Get a range of headers by the specified range.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn get_header_range(
        &self,
        range: Range<u32>,
    ) -> Result<BTreeMap<u32, Header>, FetchHeaderError> {
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Result<BTreeMap<u32, Header>, FetchHeaderError>>();
        let message = BatchHeaderRequest::new(tx, range);
        self.ntx
            .send(ClientMessage::GetHeaderBatch(message))
            .map_err(|_| FetchHeaderError::SendError)?;
        rx.await.map_err(|_| FetchHeaderError::RecvError)?
    }

    /// Request a block be fetched. Note that this method will request a block
    /// from a connected peer's inventory, and may take an indefinite amount of
    /// time, until a peer responds.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn get_block(&self, block_hash: BlockHash) -> Result<IndexedBlock, FetchBlockError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<IndexedBlock, FetchBlockError>>();
        let message = BlockRequest::new(tx, block_hash);
        self.ntx
            .send(ClientMessage::GetBlock(message))
            .map_err(|_| FetchBlockError::SendError)?;
        rx.await.map_err(|_| FetchBlockError::RecvError)?
    }

    /// Request a block be fetched and receive a [`tokio::sync::oneshot::Receiver`]
    /// to await the resulting block.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn request_block(
        &self,
        block_hash: BlockHash,
    ) -> Result<oneshot::Receiver<Result<IndexedBlock, FetchBlockError>>, FetchBlockError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<IndexedBlock, FetchBlockError>>();
        let message = BlockRequest::new(tx, block_hash);
        self.ntx
            .send(ClientMessage::GetBlock(message))
            .map_err(|_| FetchBlockError::SendError)?;
        Ok(rx)
    }

    /// Starting after the configured checkpoint, look for block inclusions with newly added scripts.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn rescan(&self) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::Rescan)
            .map_err(|_| ClientError::SendError)
    }

    /// Set a new connection timeout for peers to respond to messages.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn set_response_timeout(&self, duration: Duration) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::SetDuration(duration))
            .map_err(|_| ClientError::SendError)
    }

    /// Add another known peer to connect to.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn add_peer(&self, peer: impl Into<TrustedPeer>) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::AddPeer(peer.into()))
            .map_err(|_| ClientError::SendError)
    }

    /// Check if the node is running.
    pub fn is_running(&self) -> bool {
        self.ntx.send(ClientMessage::NoOp).is_ok()
    }
}

impl<T> From<mpsc::error::SendError<T>> for ClientError {
    fn from(_: mpsc::error::SendError<T>) -> Self {
        ClientError::SendError
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{consensus::deserialize, Transaction};
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn test_client_works() {
        let transaction: Transaction = deserialize(&hex::decode("0200000001aad73931018bd25f84ae400b68848be09db706eac2ac18298babee71ab656f8b0000000048473044022058f6fc7c6a33e1b31548d481c826c015bd30135aad42cd67790dab66d2ad243b02204a1ced2604c6735b6393e5b41691dd78b00f0c5942fb9f751856faa938157dba01feffffff0280f0fa020000000017a9140fb9463421696b82c833af241c78c17ddbde493487d0f20a270100000017a91429ca74f8a08f81999428185c97b5d852e4063f618765000000").unwrap()).unwrap();
        let (log_tx, log_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (_, info_rx) = tokio::sync::mpsc::channel::<Info>(1);
        let (_, warn_rx) = tokio::sync::mpsc::unbounded_channel::<Warning>();
        let (_, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let (ctx, crx) = mpsc::unbounded_channel::<ClientMessage>();
        let Client {
            requester,
            mut log_rx,
            info_rx: _,
            warn_rx: _,
            event_rx: _,
        } = Client::new(log_rx, info_rx, warn_rx, event_rx, ctx);
        let send_res = log_tx.send("An important message".into()).await;
        assert!(send_res.is_ok());
        let message = log_rx.recv().await;
        assert!(message.is_some());
        tokio::task::spawn(async move { log_tx.send("Another important message".into()).await });
        assert!(send_res.is_ok());
        let message = log_rx.recv().await;
        assert!(message.is_some());
        drop(log_rx);
        let broadcast = requester.broadcast_tx(TxBroadcast::new(
            transaction.clone(),
            crate::TxBroadcastPolicy::AllPeers,
        ));
        assert!(broadcast.is_ok());
        drop(crx);
        let broadcast = requester.shutdown();
        assert!(broadcast.is_err());
    }
}
