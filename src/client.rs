use bitcoin::{Amount, Transaction};
use bitcoin::{BlockHash, FeeRate};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::chain::block_subsidy;
use crate::messages::ClientRequest;
use crate::{Event, Info, TrustedPeer, TxBroadcast, Warning};

use super::{error::FetchBlockError, IndexedBlock};
use super::{
    error::{ClientError, FetchFeeRateError},
    messages::ClientMessage,
};

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
    ) -> Self {
        Self {
            requester: Requester::new(ntx),
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
        let request = ClientRequest::new((), tx);
        self.ntx
            .send(ClientMessage::GetBroadcastMinFeeRate(request))
            .map_err(|_| FetchFeeRateError::SendError)?;
        rx.await.map_err(|_| FetchFeeRateError::RecvError)
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
        let fee_rate = block_fees.to_sat() / weight.to_kwu_floor();
        Ok(FeeRate::from_sat_per_kwu(fee_rate))
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
        let (_, info_rx) = tokio::sync::mpsc::channel::<Info>(1);
        let (_, warn_rx) = tokio::sync::mpsc::unbounded_channel::<Warning>();
        let (_, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let (ctx, crx) = mpsc::unbounded_channel::<ClientMessage>();
        let Client {
            requester,
            info_rx: _,
            warn_rx: _,
            event_rx: _,
        } = Client::new(info_rx, warn_rx, event_rx, ctx);
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
