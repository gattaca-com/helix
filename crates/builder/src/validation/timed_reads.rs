use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use ethrex_common::{
    Address, H256, U256,
    types::{AccountState, ChainConfig, Code, CodeMetadata},
};
use ethrex_levm::{db::Database, errors::DatabaseError, precompiles::PrecompileCache};
use flux::timing::{Duration, Instant};

use crate::metrics;

#[derive(Default)]
struct ReadTimer {
    reads: AtomicU64,
    ticks: AtomicU64,
}

impl ReadTimer {
    fn time<T>(&self, reads: usize, read: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = read();
        self.ticks.fetch_add(start.elapsed().0, Ordering::Relaxed);
        self.reads.fetch_add(reads as u64, Ordering::Relaxed);
        out
    }

    fn record(&self, kind: &str) {
        let ticks = Duration(self.ticks.load(Ordering::Relaxed));
        metrics::sim_state_reads(kind, self.reads.load(Ordering::Relaxed), ticks.as_micros());
    }
}

pub struct TimedReads {
    inner: Arc<dyn Database>,
    account: ReadTimer,
    storage: ReadTimer,
    code: ReadTimer,
    code_metadata: ReadTimer,
    block_hash: ReadTimer,
}

impl TimedReads {
    pub fn wrap(inner: Arc<dyn Database>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            account: ReadTimer::default(),
            storage: ReadTimer::default(),
            code: ReadTimer::default(),
            code_metadata: ReadTimer::default(),
            block_hash: ReadTimer::default(),
        })
    }

    pub fn record(&self) {
        self.account.record("account");
        self.storage.record("storage");
        self.code.record("code");
        self.code_metadata.record("code_metadata");
        self.block_hash.record("block_hash");
    }
}

impl Database for TimedReads {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        self.account.time(1, || self.inner.get_account_state(address))
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        self.storage.time(1, || self.inner.get_storage_value(address, key))
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        self.block_hash.time(1, || self.inner.get_block_hash(block_number))
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.inner.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        self.code.time(1, || self.inner.get_account_code(code_hash))
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        self.code_metadata.time(1, || self.inner.get_code_metadata(code_hash))
    }

    fn precompile_cache(&self) -> Option<&PrecompileCache> {
        self.inner.precompile_cache()
    }

    fn get_account_states_batch(
        &self,
        addresses: &[Address],
    ) -> Result<Vec<AccountState>, DatabaseError> {
        self.account.time(addresses.len(), || self.inner.get_account_states_batch(addresses))
    }

    fn get_account_codes_batch(
        &self,
        code_hashes: &[H256],
    ) -> Result<Vec<Option<Code>>, DatabaseError> {
        self.code.time(code_hashes.len(), || self.inner.get_account_codes_batch(code_hashes))
    }

    fn get_storage_values_batch(
        &self,
        keys: &[(Address, H256)],
    ) -> Result<Vec<U256>, DatabaseError> {
        self.storage.time(keys.len(), || self.inner.get_storage_values_batch(keys))
    }

    fn code_cache_budget_bytes(&self) -> u64 {
        self.inner.code_cache_budget_bytes()
    }

    fn prefetch_codes(&self, code_hashes: &[H256]) -> Result<u64, DatabaseError> {
        self.code.time(code_hashes.len(), || self.inner.prefetch_codes(code_hashes))
    }

    fn prefetch_accounts(&self, addresses: &[Address]) -> Result<(), DatabaseError> {
        self.account.time(addresses.len(), || self.inner.prefetch_accounts(addresses))
    }

    fn prefetch_storage(&self, keys: &[(Address, H256)]) -> Result<(), DatabaseError> {
        self.storage.time(keys.len(), || self.inner.prefetch_storage(keys))
    }
}
