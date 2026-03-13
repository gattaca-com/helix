use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use alloy_primitives::{Bytes, TxHash};
use futures::StreamExt;
use reth_chain_state::{CanonStateNotification, CanonStateNotificationStream};
use reth_ethereum::{
    pool::{FullTransactionEvent, PoolTransaction, TransactionPool, ValidPoolTransaction},
    rpc::eth::primitives::TransactionTrait,
};
use tokio::sync::watch::Sender;

const MAX_LIST_BYTES: usize = 8 * 1024;

pub async fn inclusion_producer<P: TransactionPool>(
    pool: P,
    mut notifications: CanonStateNotificationStream,
    published: Sender<Option<Vec<Bytes>>>,
) {
    // Maintain 2 collections - 1 ordered by Inclusion List score, with `TxHash` as value. The other
    // mapping `TxHash` to tx.
    let mut ordered_txs = BTreeSet::new();
    let mut pending_txs = HashMap::new();

    let mut tx_event_listener = pool.all_transactions_event_listener();

    loop {
        tokio::select! {
            event = tx_event_listener.next() => {
                match event {
                    Some(event) => handle_tx_event(&pool, event, &mut ordered_txs, &mut pending_txs),
                    None => {
                        tracing::warn!("tx pool event listenr closed - exiting");
                        break;
                    }
                }
            },
            result = notifications.next() => {
                match result {
                    Some(update) => {
                        if let CanonStateNotification::Commit { new: _ } = update {
                            // Publish new inclusion list.
                            build_inclusion_list::<P>(&mut ordered_txs, &mut pending_txs, &published);
                        }
                    }
                    None => {
                        tracing::error!("state notifications closed - exiting");
                        break;
                    }
                }

            },
        }
    }
}

fn handle_tx_event<P: TransactionPool>(
    pool: &P,
    event: FullTransactionEvent<P::Transaction>,
    ordered_tx: &mut BTreeSet<OrderedTx>,
    pending_txs: &mut HashMap<TxHash, Arc<ValidPoolTransaction<P::Transaction>>>,
) {
    match event {
        FullTransactionEvent::Pending(tx_hash) => {
            // Add to valid set
            if let Some(pending_tx) = pool.get(&tx_hash) {
                if matches!(pending_tx.transaction.blob_count(), Some(0) | None) {
                    let tx_hash = *pending_tx.hash();
                    let score = score_tx(&pending_tx);
                    ordered_tx.insert(OrderedTx { score, tx_hash });
                    pending_txs.insert(tx_hash, pending_tx);
                }
            }
        }
        FullTransactionEvent::Queued(tx_hash, _) |
        FullTransactionEvent::Mined { tx_hash, .. } |
        FullTransactionEvent::Discarded(tx_hash) |
        FullTransactionEvent::Invalid(tx_hash) => {
            // Remove from tx mapping.
            pending_txs.remove(&tx_hash);
        }
        FullTransactionEvent::Replaced { transaction, replaced_by: _ } => {
            // Remove
            let tx_hash = transaction.hash();
            pending_txs.remove(tx_hash);
        }
        FullTransactionEvent::Propagated(_) => {}
    }
}

fn build_inclusion_list<P: TransactionPool>(
    ordered_txs: &mut BTreeSet<OrderedTx>,
    pending_txs: &mut HashMap<TxHash, Arc<ValidPoolTransaction<P::Transaction>>>,
    published: &Sender<Option<Vec<Bytes>>>,
) {
    tracing::info!("building new inclusion list");

    let mut inclusion_list = vec![];
    let mut to_add = vec![];
    let mut total_encoded_size = 0;

    while let Some(ordered_tx) = ordered_txs.pop_first() {
        if let Some(transaction) = pending_txs.get(&ordered_tx.tx_hash) {
            to_add.push(ordered_tx); // will add back to the ordered txs. 
            if total_encoded_size + transaction.encoded_length() > MAX_LIST_BYTES {
                continue;
            }
            total_encoded_size += transaction.encoded_length();
            let rlp_encoded_tx = transaction.to_consensus().into_encoded();
            inclusion_list.push(rlp_encoded_tx.into_encoded_bytes());
        }
    }
    let _ = published.send(Some(inclusion_list));
    ordered_txs.extend(to_add);
}

#[derive(Debug)]
struct OrderedTx {
    score: f64,
    tx_hash: TxHash,
}

impl PartialEq for OrderedTx {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for OrderedTx {}

impl PartialOrd for OrderedTx {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedTx {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse ordering b/c highest scores come first.
        self.score.total_cmp(&other.score).reverse()
    }
}

fn score_tx<T: PoolTransaction>(tx: &Arc<ValidPoolTransaction<T>>) -> f64 {
    let tx_added_ts = tx.timestamp;
    let tx_gas_limit = tx.gas_limit();
    let tx_priority_fee = tx.priority_fee_or_price();

    (tx_added_ts.elapsed().as_micros() as f64 * tx_priority_fee as f64) / tx_gas_limit as f64
}
