use std::time::Duration;

use bitcoin::p2p::address::AddrV2;
use bitcoin::p2p::ServiceFlags;
use bitcoin::{Amount, Transaction, Wtxid};
use bitcoin::{BlockHash, FeeRate};
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::chain::block_subsidy;
use crate::chain::IndexedHeader;
use crate::messages::ClientRequest;
use crate::{Event, HeaderCheckpoint, Info, TrustedPeer, Warning};

use super::{error::ClientError, messages::ClientMessage};
use super::{error::FetchBlockError, IndexedBlock};

/// A [`Client`] allows for communication with a running node.
#[derive(Debug)]
pub struct Client {
    /// Send events to a node, such as broadcasting a transaction.
    pub requester: Requester,
    /// Receive informational messages from the node.
    pub info_rx: mpsc::Receiver<Info>,
    /// Receive warning messages from a node.
    pub warn_rx: mpsc::UnboundedReceiver<Warning>,
    /// Receive [`Event`] from a node to act on.
    pub event_rx: mpsc::UnboundedReceiver<Event>,
}

impl Client {
    pub(crate) fn new(
        info_rx: mpsc::Receiver<Info>,
        warn_rx: mpsc::UnboundedReceiver<Warning>,
        event_rx: mpsc::UnboundedReceiver<Event>,
        ntx: UnboundedSender<ClientMessage>,
        broadcast_timeout: Option<Duration>,
    ) -> Self {
        Self {
            requester: Requester::new(ntx, broadcast_timeout),
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
    broadcast_timeout: Option<Duration>,
}

impl Requester {
    fn new(ntx: UnboundedSender<ClientMessage>, broadcast_timeout: Option<Duration>) -> Self {
        Self {
            ntx,
            broadcast_timeout,
        }
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

    /// Broadcast a new transaction to the network, waiting for at least one peer to request it.
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
    /// If the node has stopped running or if the configured broadcast timeout expires before
    /// any peer requests the transaction. A timeout does not necessarily mean the broadcast
    /// failed — the peer may already have the transaction and simply not sent a `getdata` request.
    pub async fn broadcast_tx(&self, transaction: Transaction) -> Result<Wtxid, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Wtxid>();
        let client_request = ClientRequest::new(transaction, tx);
        self.ntx
            .send(ClientMessage::Broadcast(client_request))
            .map_err(|_| ClientError::SendError)?;
        match self.broadcast_timeout {
            Some(timeout) => tokio::time::timeout(timeout, rx)
                .await
                .map_err(|_| ClientError::BroadcastTimeout)?
                .map_err(|_| ClientError::RecvError),
            None => rx.await.map_err(|_| ClientError::RecvError),
        }
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
    pub async fn broadcast_min_feerate(&self) -> Result<FeeRate, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<FeeRate>();
        let request = ClientRequest::new((), tx);
        self.ntx
            .send(ClientMessage::GetBroadcastMinFeeRate(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
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
        let message = ClientRequest::new(block_hash, tx);
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
        let message = ClientRequest::new(block_hash, tx);
        self.ntx
            .send(ClientMessage::GetBlock(message))
            .map_err(|_| FetchBlockError::SendError)?;
        Ok(rx)
    }

    /// Fetch the average fee rate for the given block hash.
    ///
    /// Computed by taking (`coinbase output amount` - `block subsidy`) / `block weight`. Note that
    /// this value may provide skewed estimates, as averages are more effected by outliers than
    /// medians. For a rudimentary estimation of the fee rate required to enter the next block,
    /// this method may suffice.
    pub async fn average_fee_rate(
        &self,
        block_hash: BlockHash,
    ) -> Result<FeeRate, FetchBlockError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<IndexedBlock, FetchBlockError>>();
        let message = ClientRequest::new(block_hash, tx);
        self.ntx
            .send(ClientMessage::GetBlock(message))
            .map_err(|_| FetchBlockError::SendError)?;
        let indexed_block = rx.await.map_err(|_| FetchBlockError::RecvError)??;
        let subsidy = block_subsidy(indexed_block.height);
        let weight = indexed_block.block.weight();
        let revenue = indexed_block
            .block
            .txdata
            .first()
            .map(|tx| tx.output.iter().map(|txout| txout.value).sum())
            .unwrap_or(Amount::ZERO);
        let block_fees = revenue.checked_sub(subsidy).unwrap_or(Amount::ZERO);
        let fee_rate = block_fees
            .to_sat()
            .checked_div(weight.to_kwu_floor())
            .unwrap_or(0);
        Ok(FeeRate::from_sat_per_kwu(fee_rate))
    }

    /// Get the current peer connections.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn peer_info(&self) -> Result<Vec<(AddrV2, ServiceFlags)>, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<(AddrV2, ServiceFlags)>>();
        let request = ClientRequest::new((), tx);
        self.ntx
            .send(ClientMessage::GetPeerInfo(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
    }

    /// Starting after the configured checkpoint, re-emit all block filters.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn rescan(&self) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::Rescan)
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

    /// The height and hash of the block in the chain of most work.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn chain_tip(&self) -> Result<HeaderCheckpoint, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<HeaderCheckpoint>();
        let request = ClientRequest::new((), tx);
        self.ntx
            .send(ClientMessage::BestBlock(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
    }

    /// Look up a header at a specific height in the locally synced chain of most work.
    /// Returns `None` if the height is not in the header chain.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn get_header(&self, height: u32) -> Result<Option<IndexedHeader>, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<IndexedHeader>>();
        let request = ClientRequest::new(height, tx);
        self.ntx
            .send(ClientMessage::GetHeader(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
    }

    /// Look up the height of a block hash in the locally synced chain of most work.
    /// Returns `None` if the hash is not in the chain of most work.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn height_of_hash(&self, hash: BlockHash) -> Result<Option<u32>, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<u32>>();
        let request = ClientRequest::new(hash, tx);
        self.ntx
            .send(ClientMessage::HeightOfHash(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
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
