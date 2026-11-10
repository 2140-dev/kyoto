use bitcoin::p2p::address::AddrV2;
use bitcoin::p2p::ServiceFlags;
use bitcoin::{Amount, Transaction, Wtxid};
use bitcoin::{BlockHash, FeeRate};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::chain::block_subsidy;
use crate::chain::IndexedHeader;
use crate::messages::ClientRequest;
use crate::{
    BlockType, Event, GossipMonitorRequest, HashCheckpoint, Info, Package, TrustedPeer, Warning,
};

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
    pub warn_rx: mpsc::Receiver<Warning>,
    /// Receive [`Event`] from a node to act on.
    pub event_rx: EventReceiver,
}

impl Client {
    pub(crate) fn new(
        info_rx: mpsc::Receiver<Info>,
        warn_rx: mpsc::Receiver<Warning>,
        event_rx: EventReceiver,
        ntx: UnboundedSender<ClientMessage>,
    ) -> Self {
        Self {
            requester: Requester::new(ntx),
            info_rx,
            warn_rx,
            event_rx,
        }
    }
}

/// Receive [`Event`] from a node.
#[derive(Debug)]
pub struct EventReceiver {
    rx: mpsc::UnboundedReceiver<Event>,
    pending_events: Arc<AtomicUsize>,
}

// This wrapper class tells us when a client has removed an event from the channel. Filters can be
// quite large, so before we request them we make sure the client is not behind.
impl EventReceiver {
    pub(crate) fn new(
        rx: mpsc::UnboundedReceiver<Event>,
        pending_events: Arc<AtomicUsize>,
    ) -> Self {
        Self { rx, pending_events }
    }

    /// Receive the next event, or `None` if the node has stopped. This method is cancel safe.
    pub async fn recv(&mut self) -> Option<Event> {
        let event = self.rx.recv().await;
        if event.is_some() {
            self.pending_events.fetch_sub(1, Ordering::Relaxed);
        }
        event
    }

    /// Try to receive the next event without waiting.
    pub fn try_recv(&mut self) -> Result<Event, TryRecvError> {
        let event = self.rx.try_recv()?;
        self.pending_events.fetch_sub(1, Ordering::Relaxed);
        Ok(event)
    }

    /// Poll to receive the next event, or `None` if the node has stopped.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        let poll = self.rx.poll_recv(cx);
        if let Poll::Ready(Some(_)) = poll {
            self.pending_events.fetch_sub(1, Ordering::Relaxed);
        }
        poll
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

    /// Submit a package of transactions to the network, returning when transaction data was sent
    /// to at least one peer.
    ///
    /// Note that this is directly callable with a single [`Transaction`].
    ///
    /// # Returns
    ///
    /// The `Wtxid` of the child or singleton transaction.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub async fn submit_package(&self, package: impl Into<Package>) -> Result<Wtxid, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Wtxid>();
        let package = package.into();
        let client_request = ClientRequest::new(package, tx);
        self.ntx
            .send(ClientMessage::Broadcast(client_request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
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
        let message = ClientRequest::new((block_hash, None), tx);
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
        let message = ClientRequest::new((block_hash, None), tx);
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
        let message = ClientRequest::new((block_hash, Some(BlockType::Witness)), tx);
        self.ntx
            .send(ClientMessage::GetBlock(message))
            .map_err(|_| FetchBlockError::SendError)?;
        let indexed_block = rx.await.map_err(|_| FetchBlockError::RecvError)??;
        if !indexed_block.block.check_witness_commitment() {
            return Err(FetchBlockError::InvalidWitnessCommitment);
        }
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
            .send(ClientMessage::Rescan(None))
            .map_err(|_| ClientError::SendError)
    }

    /// Re-emit block filters _after_ the specified height.
    ///
    /// # Errors
    ///
    /// If the node has stopped running.
    pub fn rescan_from(&self, height: u32) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::Rescan(Some(height)))
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
    pub async fn chain_tip(&self) -> Result<HashCheckpoint, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<HashCheckpoint>();
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

    /// Subscribe to transaction gossip that involves scripts or outpoints of interest. This
    /// feature is considered **experimental**.
    ///
    /// # Behavior
    ///
    /// When called for the first time, this will create a _new_ peer-to-peer connection for the
    /// sole purpose of finding transactions that involve scripts and outpoints provided in the
    /// request. When called subsequent times, the scripts or outpoints provided will be added to
    /// the existing set.
    ///
    /// # Properties
    ///
    /// - No loss of privacy: all transactions are annouced by `inv` messages, so the node will
    ///   receive all transaction gossip and does not need to request special data that would
    ///   otherwise reveal information.
    /// - Unreliable: remote nodes are not obligated to forward unconfirmed transactions.
    ///   Transactions may exist involving the scripts and outpoints, but the peer may not forward
    ///   them.
    /// - Untrustworthy: unconfirmed transactions may not be treated as final! It does not take any
    ///   proof of work to generate transactions. No claims on transaction validity are possible
    ///   until the transaction has been included in a block of sufficient work.
    ///
    /// ## When this feature makes sense to use
    ///
    /// - You are expecting to see a payment from a trusted counterparty, for instance an exchange,
    ///   broker, friend, or colleague.
    ///
    /// ## When this feature does not make sense
    ///
    /// - You are expecting to see a payment from an untrusted counterparty.
    ///
    /// # Returns
    ///
    /// On first subscription, this function an [`mpsc::Receiver`] that will forward any transaction
    /// matching the scripts and outpoints provided. Note, duplicate transactions may be sent. On
    /// subsequent subscriptions, this method returns `None`, as there is an exisiting
    /// [`mpsc::Receiver`].
    pub async fn subscribe_to_gossip(
        &self,
        req: GossipMonitorRequest,
    ) -> Result<Option<mpsc::Receiver<Transaction>>, ClientError> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<mpsc::Receiver<Transaction>>>();
        let request = ClientRequest::new(req, tx);
        self.ntx
            .send(ClientMessage::SubscribeGossip(request))
            .map_err(|_| ClientError::SendError)?;
        rx.await.map_err(|_| ClientError::RecvError)
    }

    /// Unsubscribe from all transaction gossip.
    pub fn unsubscribe_from_gossip(&self) -> Result<(), ClientError> {
        self.ntx
            .send(ClientMessage::UnsubscribeGossip)
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
