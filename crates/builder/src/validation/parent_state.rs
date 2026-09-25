use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use ethrex_blockchain::vm::StoreVmDatabase;
use ethrex_common::{
    H256,
    types::{BlockHeader, Fork},
};
use ethrex_levm::db::CachingDatabase;
use ethrex_storage::Store;
use ethrex_vm::{DynVmDatabase, EvmError};

const MAX_PARENTS: usize = 4;

/// Precompile results depend on the fork, so a parent seen under two forks gets two entries.
struct ParentState {
    parent_hash: H256,
    fork: Fork,
    vm_db: StoreVmDatabase,
    reads: Arc<CachingDatabase>,
}

/// Every cached read is a pure function of the parent's state, so its validations share one.
#[derive(Default)]
pub struct ParentStateCache {
    entries: Mutex<VecDeque<ParentState>>,
}

impl ParentStateCache {
    pub fn get(
        &self,
        store: &Store,
        parent_hash: H256,
        parent_header: &BlockHeader,
        fork: Fork,
    ) -> Result<(StoreVmDatabase, Arc<CachingDatabase>), EvmError> {
        let mut entries =
            self.entries.lock().map_err(|_| EvmError::Custom("lock poisoned".into()))?;
        if let Some(entry) =
            entries.iter().find(|entry| entry.parent_hash == parent_hash && entry.fork == fork)
        {
            return Ok((entry.vm_db.clone(), entry.reads.clone()));
        }

        let vm_db = StoreVmDatabase::new(store.clone(), parent_header.clone())?;
        let inner: DynVmDatabase = Box::new(vm_db.clone());
        let reads = Arc::new(CachingDatabase::new(Arc::new(inner), true));
        if entries.len() == MAX_PARENTS {
            entries.pop_front();
        }
        entries.push_back(ParentState {
            parent_hash,
            fork,
            vm_db: vm_db.clone(),
            reads: reads.clone(),
        });
        Ok((vm_db, reads))
    }
}
