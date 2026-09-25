use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc::Sender,
};

use crossbeam_channel::Receiver;
use ethrex_blockchain::Blockchain;
use ethrex_common::types::AccountUpdate;
use ethrex_storage::Store;
use ethrex_vm::Evm;

use crate::validation::error::ValidationError;

const FLUSH_AFTER_TXS: usize = 5;

pub struct MerklePools {
    idle: (crossbeam_channel::Sender<Arc<Blockchain>>, Receiver<Arc<Blockchain>>),
}

impl MerklePools {
    pub fn new(store: &Store, count: usize) -> Self {
        let idle = crossbeam_channel::bounded(count);
        for _ in 0..count {
            let _ = idle.0.try_send(Arc::new(Blockchain::default_with_store_and_pool(
                store.clone(),
                Blockchain::build_merkle_pool(),
            )));
        }
        Self { idle }
    }

    pub fn checkout(&self) -> Option<MerklePool<'_>> {
        let blockchain = self.idle.1.try_recv().ok()?;
        Some(MerklePool { pools: self, blockchain })
    }
}

pub struct MerklePool<'a> {
    pools: &'a MerklePools,
    pub blockchain: Arc<Blockchain>,
}

impl Drop for MerklePool<'_> {
    fn drop(&mut self) {
        let _ = self.pools.idle.0.try_send(self.blockchain.clone());
    }
}

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
