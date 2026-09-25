use std::sync::Arc;

use dashmap::DashMap;
use ethrex_blockchain::vm::StoreVmDatabase;
use ethrex_common::{
    H256,
    types::{BlockHeader, Fork},
};
use ethrex_levm::db::CachingDatabase;
use ethrex_storage::Store;
use ethrex_vm::{DynVmDatabase, EvmError};

#[derive(Default)]
pub struct ParentStateCache {
    entries: DashMap<(H256, Fork), (u64, StoreVmDatabase, Arc<CachingDatabase>)>,
}

impl ParentStateCache {
    pub fn get(
        &self,
        store: &Store,
        parent_hash: H256,
        parent_header: &BlockHeader,
        fork: Fork,
        oldest: u64,
    ) -> Result<(StoreVmDatabase, Arc<CachingDatabase>), EvmError> {
        if let Some(entry) = self.entries.get(&(parent_hash, fork)) {
            return Ok((entry.1.clone(), entry.2.clone()));
        }
        let vm_db = StoreVmDatabase::new(store.clone(), parent_header.clone())?;
        let inner: DynVmDatabase = Box::new(vm_db.clone());
        let reads = Arc::new(CachingDatabase::new(Arc::new(inner), true));
        self.entries.retain(|_, entry| entry.0 >= oldest);
        self.entries
            .insert((parent_hash, fork), (parent_header.number, vm_db.clone(), reads.clone()));
        Ok((vm_db, reads))
    }
}
