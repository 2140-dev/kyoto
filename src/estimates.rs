use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use bitcoin::Amount;
use bitcoin::Block;
use bitcoin::FeeRate;
use bitcoin::Transaction;
use bitcoin::Txid;
use bitcoin::Weight;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::channel_messages::FetchTransactions;
use crate::channel_messages::MainThreadMessage;
use crate::dialog::Dialog;
use crate::network::peer_map::PeerMap;
use crate::PeerStore;
use crate::Warning;

const CHANNEL_SIZE: usize = 10_000;

#[derive(Debug)]
struct PendingFeeRate {
    outpoints: HashMap<Txid, u32>,
    input_value: Amount,
    output_value: Amount,
    weight: Weight,
}

impl PendingFeeRate {
    // The `Txid` is precomputed when receiving the transaction off the wire.
    // When all outpoints have been fetched this returns the final feerate.
    fn update(&mut self, txid: &Txid, transaction: &Transaction) -> Option<FeeRate> {
        if let Some(index) = self.outpoints.remove(txid) {
            let index = index as usize;
            // This should not fail as we expect the block is valid.
            if let Some(output) = transaction.output.get(index) {
                self.input_value += output.value;
            }
            if self.outpoints.is_empty() {
                let delta = self
                    .input_value
                    .to_sat()
                    .saturating_sub(self.output_value.to_sat());
                let weight_kwu = self.weight.to_kwu_floor();
                let sat_kwu = delta.checked_div(weight_kwu).unwrap_or(0);
                let feerate = FeeRate::from_sat_per_kwu(sat_kwu);
                // If the feerate is somehow lower than current policy, return the broadcast
                // minimum instead.
                return Some(feerate.max(FeeRate::BROADCAST_MIN));
            } else {
                return None;
            }
        }
        None
    }
}

// We keep a map of what transaction an outpoint was spent in, so we can fetch the corresponding
// fee rate. The outpoint `Txid` alone is not enough to determine where in the block it was spent.
type OutPointToSpend = HashMap<Txid, Txid>;
type SpendToPendingFeeRate = HashMap<Txid, PendingFeeRate>;

// Strip all unnecessary information from a block when computing fee rates
fn block_to_pending_feerates(block: Block) -> (OutPointToSpend, SpendToPendingFeeRate) {
    let capacity = block.txdata.len();
    let mut outpoint_to_spend = HashMap::with_capacity(capacity);
    let mut spent_to_pending = HashMap::with_capacity(capacity);
    for tx in block.txdata.into_iter().skip(1) {
        let txid = tx.compute_txid();
        let weight = tx.weight();
        let output_value = tx
            .output
            .into_iter()
            .map(|tx_out| tx_out.value)
            .sum::<Amount>();
        let mut index_map = HashMap::with_capacity(tx.input.len());
        for input in tx.input {
            outpoint_to_spend.insert(input.previous_output.txid, txid);
            index_map.insert(input.previous_output.txid, input.previous_output.vout);
        }
        let pending_fees = PendingFeeRate {
            outpoints: index_map,
            input_value: Amount::ZERO,
            output_value,
            weight,
        };
        spent_to_pending.insert(txid, pending_fees);
    }
    (outpoint_to_spend, spent_to_pending)
}

pub(crate) async fn get_fees_for_block<P: PeerStore + 'static>(
    peer_map: Arc<Mutex<PeerMap<P>>>,
    block_receiver: oneshot::Receiver<Block>,
    request: oneshot::Sender<Vec<FeeRate>>,
    dialog: Arc<Dialog>,
) {
    let then = Instant::now();
    crate::log!(dialog, "Waiting for block data to begin fee calculation");
    let block_res = block_receiver.await;
    let (block, mut transaction_rx) = match block_res {
        Ok(block) => {
            let outpoint_txids: HashSet<Txid> = block
                .txdata
                .iter()
                .skip(1)
                .map(|tx| {
                    tx.input
                        .iter()
                        .map(|txin| txin.previous_output.txid)
                        .collect::<HashSet<Txid>>()
                })
                .reduce(|mut acc, set| {
                    acc.extend(set);
                    acc
                })
                .unwrap_or(HashSet::new());
            if outpoint_txids.is_empty() {
                crate::log!(dialog, "Cannot estimate fees, block is empty");
                return;
            }
            let mut map = peer_map.lock().await;
            let (transaction_tx, transaction_rx) =
                mpsc::channel::<(Txid, Transaction)>(CHANNEL_SIZE);
            let request = MainThreadMessage::FetchTransactions(FetchTransactions::new(
                transaction_tx,
                outpoint_txids,
            ));
            let could_request = map.broadcast(request).await;
            if !could_request {
                crate::log!(
                    dialog,
                    "Failed to calculate fee rates, no connected peers were available"
                );
                return;
            }
            (block, transaction_rx)
        }
        Err(_) => {
            crate::log!(dialog, "Failed to receive block to begin fee calculation");
            dialog.send_warning(Warning::ChannelDropped);
            return;
        }
    };
    crate::log!(dialog, "Starting fee rate calculations");
    let mut feerates = Vec::with_capacity(block.txdata.len());
    let (outpoint_to_spend, mut spend_to_pending) = block_to_pending_feerates(block);
    while let Some((outpoint_txid, transaction)) = transaction_rx.recv().await {
        // This should not miss as no outpoint -> spend relationship is removed
        if let Some(spending_txid) = outpoint_to_spend.get(&outpoint_txid) {
            // If this misses the feereate is no longer pending
            if let Some(pending_fee_rate) = spend_to_pending.get_mut(spending_txid) {
                let feerate = pending_fee_rate.update(&outpoint_txid, &transaction);
                if let Some(feerate) = feerate {
                    feerates.push(feerate);
                    spend_to_pending.remove(spending_txid);
                    if spend_to_pending.is_empty() {
                        let fetch_time = then.elapsed();
                        crate::log!(
                            dialog,
                            format!(
                                "Fee rate fetch complete in {} seconds",
                                fetch_time.as_secs_f32()
                            )
                        );
                        if request.send(feerates).is_err() {
                            crate::log!(dialog, "Failed to send fee rates to client.");
                            dialog.send_warning(Warning::ChannelDropped);
                        }
                        return;
                    }
                }
            }
        }
    }
}
