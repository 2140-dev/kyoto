use std::collections::HashMap;

use bitcoin::{Transaction, Wtxid};
use tokio::sync::oneshot;

#[derive(Debug)]
pub(crate) struct BroadcastQueue {
    pending: HashMap<Wtxid, oneshot::Sender<Wtxid>>,
    data: HashMap<Wtxid, Transaction>,
}

impl BroadcastQueue {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            data: HashMap::new(),
        }
    }

    pub(crate) fn add_to_queue(&mut self, tx: Transaction, oneshot: oneshot::Sender<Wtxid>) {
        let wtxid = tx.compute_wtxid();
        self.pending.insert(wtxid, oneshot);
        self.data.insert(wtxid, tx);
    }

    pub(crate) fn fetch_tx(&self, wtxid: Wtxid) -> Option<Transaction> {
        self.data.get(&wtxid).cloned()
    }

    pub(crate) fn successful(&mut self, wtxid: Wtxid) {
        if let Some(pending) = self.pending.remove(&wtxid) {
            let _ = pending.send(wtxid);
        }
    }

    pub(crate) fn pending_wtxid(&self) -> Vec<Wtxid> {
        self.pending.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use bitcoin::Transaction;
    use corepc_node::serde_json;

    use super::BroadcastQueue;

    #[derive(Debug, Clone)]
    struct HexTx(Transaction);
    crate::impl_deserialize!(HexTx, Transaction);

    #[derive(Debug, Clone, serde::Deserialize)]
    struct TransactionFile {
        transactions: Vec<HexTx>,
    }

    #[test]
    fn test_broadcast_queue_works() {
        // Sourced from BIP 174 test vectors
        let tx_file = File::open("./tests/data/transactions.json").unwrap();
        let tx_data: TransactionFile = serde_json::from_reader(&tx_file).unwrap();
        let transaction_1: Transaction = tx_data.transactions[0].clone().0;
        let transaction_2: Transaction = tx_data.transactions[1].clone().0;
        let mut queue = BroadcastQueue::new();
        let (tx, _) = tokio::sync::oneshot::channel();
        queue.add_to_queue(transaction_1.clone(), tx);
        let (tx, _) = tokio::sync::oneshot::channel();
        queue.add_to_queue(transaction_2.clone(), tx);
        assert_eq!(queue.pending_wtxid().len(), 2);
        queue.successful(transaction_1.compute_wtxid());
        assert_eq!(queue.pending_wtxid().len(), 1);
        assert!(queue.fetch_tx(transaction_1.compute_wtxid()).is_some());
        assert!(queue.fetch_tx(transaction_2.compute_wtxid()).is_some());
        queue.successful(transaction_2.compute_wtxid());
        assert_eq!(queue.pending_wtxid().len(), 0);
    }
}
