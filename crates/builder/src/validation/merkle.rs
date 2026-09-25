use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
};

use ethrex_blockchain::Blockchain;
use ethrex_common::types::AccountUpdate;
use ethrex_storage::Store;
use ethrex_vm::Evm;

use crate::validation::error::ValidationError;

const FLUSH_AFTER_TXS: usize = 5;

/// One pool per concurrent validation: two merkleizations sharing a pool can deadlock.
pub struct MerklePools {
    idle: Mutex<Vec<Arc<Blockchain>>>,
}

impl MerklePools {
    pub fn new(store: &Store, count: usize) -> Self {
        let idle = (0..count)
            .map(|_| {
                Arc::new(Blockchain::default_with_store_and_pool(
                    store.clone(),
                    Blockchain::build_merkle_pool(),
                ))
            })
            .collect();
        Self { idle: Mutex::new(idle) }
    }

    pub fn checkout(&self) -> Option<MerklePool<'_>> {
        let blockchain = self.idle.lock().ok()?.pop()?;
        Some(MerklePool { pools: self, blockchain })
    }
}

pub struct MerklePool<'a> {
    pools: &'a MerklePools,
    pub blockchain: Arc<Blockchain>,
}

impl Drop for MerklePool<'_> {
    fn drop(&mut self) {
        if let Ok(mut idle) = self.pools.idle.lock() {
            idle.push(self.blockchain.clone());
        }
    }
}

/// ethrex's import flush rule: a batch goes out only once the merkleizer has taken the last.
pub struct UpdateStream<'a> {
    tx: Sender<Vec<AccountUpdate>>,
    queue_length: &'a AtomicUsize,
    since_flush: usize,
}

impl<'a> UpdateStream<'a> {
    pub fn new(tx: Sender<Vec<AccountUpdate>>, queue_length: &'a AtomicUsize) -> Self {
        Self { tx, queue_length, since_flush: 0 }
    }

    pub fn after_tx(&mut self, vm: &mut Evm) -> Result<(), ValidationError> {
        if self.queue_length.load(Ordering::Relaxed) == 0 && self.since_flush > FLUSH_AFTER_TXS {
            self.flush(vm)?;
            self.since_flush = 0;
        } else {
            self.since_flush += 1;
        }
        Ok(())
    }

    pub fn flush(&mut self, vm: &mut Evm) -> Result<(), ValidationError> {
        let updates = vm
            .db
            .get_state_transitions_tx()
            .map_err(|e| ValidationError::Execution(e.to_string()))?;
        self.tx
            .send(updates)
            .map_err(|_| ValidationError::Execution("merkleizer stopped early".into()))?;
        self.queue_length.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
