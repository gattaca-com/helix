//! Order presimulation. Port of the simulator's `simulate_order`
//! (`crates/simulator/src/block_merging/mod.rs:1013`) onto ethrex: each
//! candidate runs on a fresh `Evm` over a shared `PresimBase`, which gives the
//! same isolation revm's `CacheDB` wrapper did.

use std::sync::Arc;

use ethrex_common::{
    Address, H256, U256,
    constants::EMPTY_TRIE_HASH,
    types::{AccountState, BlockHeader, ChainConfig, Code, CodeMetadata},
};
use ethrex_levm::{
    account::AccountStatus,
    db::{
        Database,
        gen_db::{CacheDB, GeneralizedDatabase},
    },
    errors::DatabaseError,
    precompiles::PrecompileCache,
};
use ethrex_vm::Evm;
use flux_profiler::timed;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::engine::{
    convert::au256,
    error::SimulationError,
    reuse,
    types::{PreparedOrder, SimulatedOrder},
};

/// Cheap pre-checks shared by presim and live re-sim: duplicate txs and
/// aggregate gas/blob headroom.
#[timed]
pub fn gate_order(
    order: &PreparedOrder,
    applied_tx_hashes: &FxHashSet<alloy_primitives::B256>,
    available_gas: u64,
    available_blobs: u64,
) -> Result<(), SimulationError> {
    let any_duplicate_undroppable = order
        .txs
        .iter()
        .enumerate()
        .any(|(i, tx)| applied_tx_hashes.contains(&tx.hash) && !order.can_drop(i));
    if any_duplicate_undroppable {
        return Err(SimulationError::DuplicateTransaction);
    }

    // An order whose *minimum* footprint (undroppable txs) can't fit is not
    // worth simulating.
    let min_gas: u64 = order
        .txs
        .iter()
        .enumerate()
        .filter(|(i, _)| !order.can_drop(*i))
        .map(|(_, tx)| tx.gas_limit)
        .sum();
    if min_gas > available_gas {
        return Err(SimulationError::OutOfBlockGas);
    }
    let min_blobs: u64 = order
        .txs
        .iter()
        .enumerate()
        .filter(|(i, _)| !order.can_drop(*i))
        .map(|(_, tx)| tx.blob_hashes.len() as u64)
        .sum();
    if min_blobs > available_blobs {
        return Err(SimulationError::OutOfBlockBlobs);
    }
    Ok(())
}

/// The live context's state, frozen for one extend pass and read by every
/// presim in it. Cloning the live `Evm` per order instead copies every account
/// the base touched, which dominated presim cost.
pub struct PresimBase {
    accounts: CacheDB,
    codes: FxHashMap<H256, Code>,
    code_metadata: FxHashMap<H256, CodeMetadata>,
    store: Arc<dyn Database>,
}

impl PresimBase {
    pub fn new(db: &GeneralizedDatabase) -> Self {
        // `initial` holds the committed baseline; `current` overlays it, and its
        // storage is only the slots read or written since the account faulted in.
        let mut accounts = db.initial_accounts_state.clone();
        for (address, account) in &db.current_accounts_state {
            match accounts.get_mut(address) {
                Some(base) if !destroyed(&account.status) => {
                    base.info = account.info.clone();
                    base.status = account.status.clone();
                    base.has_storage = account.has_storage;
                    base.exists = account.exists;
                    base.storage.extend(account.storage.iter().map(|(k, v)| (*k, *v)));
                }
                _ => {
                    accounts.insert(*address, account.clone());
                }
            }
        }
        Self {
            accounts,
            codes: db.codes.clone(),
            code_metadata: db.code_metadata.clone(),
            store: db.store.clone(),
        }
    }

    /// A fresh EVM over this state, isolated from every other presim.
    pub fn evm(self: &Arc<Self>, live: &Evm) -> Evm {
        let mut db = GeneralizedDatabase::new(self.clone());
        db.keep_tx_backup = true;
        Evm { db, vm_type: live.vm_type, crypto: live.crypto.clone(), stateless_validator: None }
    }
}

impl Database for PresimBase {
    fn get_account_state(&self, address: Address) -> Result<AccountState, DatabaseError> {
        let Some(account) = self.accounts.get(&address) else {
            return self.store.get_account_state(address);
        };
        if !account.exists {
            return Ok(AccountState::default());
        }
        Ok(AccountState {
            nonce: account.info.nonce,
            balance: account.info.balance,
            // Only emptiness is read back (EIP-7610 collisions).
            storage_root: if account.has_storage { H256::zero() } else { *EMPTY_TRIE_HASH },
            code_hash: account.info.code_hash,
        })
    }

    fn get_storage_value(&self, address: Address, key: H256) -> Result<U256, DatabaseError> {
        match self.accounts.get(&address) {
            Some(account) => match account.storage.get(&key) {
                Some(value) => Ok(*value),
                None if destroyed(&account.status) => Ok(U256::zero()),
                None => self.store.get_storage_value(address, key),
            },
            None => self.store.get_storage_value(address, key),
        }
    }

    fn get_block_hash(&self, block_number: u64) -> Result<H256, DatabaseError> {
        self.store.get_block_hash(block_number)
    }

    fn get_chain_config(&self) -> Result<ChainConfig, DatabaseError> {
        self.store.get_chain_config()
    }

    fn get_account_code(&self, code_hash: H256) -> Result<Code, DatabaseError> {
        match self.codes.get(&code_hash) {
            Some(code) => Ok(code.clone()),
            None => self.store.get_account_code(code_hash),
        }
    }

    fn get_code_metadata(&self, code_hash: H256) -> Result<CodeMetadata, DatabaseError> {
        match self.code_metadata.get(&code_hash) {
            Some(metadata) => Ok(*metadata),
            None => self.store.get_code_metadata(code_hash),
        }
    }

    fn precompile_cache(&self) -> Option<&PrecompileCache> {
        self.store.precompile_cache()
    }
}

fn destroyed(status: &AccountStatus) -> bool {
    matches!(status, AccountStatus::Destroyed | AccountStatus::DestroyedModified)
}

/// Simulates an order on a fresh `vm` from `PresimBase::evm`.
/// Returns gas used, per-tx inclusion flags and the beneficiary balance delta.
///
/// The vm accumulates state as txs apply, so bundle-internal dependencies
/// resolve exactly as they would on the live state.
#[timed]
pub fn simulate_order(
    vm: &mut Evm,
    header: &BlockHeader,
    order: &PreparedOrder,
    order_ix: usize,
    available_gas: u64,
    available_blobs: u64,
    beneficiary: ethrex_common::Address,
    (results, verify, builder): (&reuse::Cache, bool, alloy_primitives::Address),
) -> Result<SimulatedOrder, SimulationError> {
    let initial_balance = balance_of(vm, beneficiary)?;

    let mut gas_used: u64 = 0;
    let mut blobs_added: u64 = 0;
    let mut include_tx = vec![true; order.txs.len()];
    let mut cumulative_gas_spent: u64 = 0;

    for (i, tx) in order.txs.iter().enumerate() {
        let can_be_dropped = order.can_drop(i);
        let can_revert = order.can_revert(i);

        // If the tx takes too much gas, try to drop it or fail.
        if tx.gas_limit > available_gas - gas_used {
            if !can_be_dropped {
                return Err(SimulationError::OutOfBlockGas);
            }
            include_tx[i] = false;
            continue;
        }
        // If the tx exceeds the blob limit, try to drop it or fail.
        if tx.blob_hashes.len() as u64 > available_blobs - blobs_added {
            if !can_be_dropped {
                return Err(SimulationError::OutOfBlockBlobs);
            }
            include_tx[i] = false;
            continue;
        }

        match reuse::presim_tx(results, verify, vm, header, tx, &mut cumulative_gas_spent, builder)
        {
            Ok((succeeded, tx_gas)) => {
                if succeeded || can_revert {
                    if gas_used + tx_gas > available_gas {
                        if !can_be_dropped {
                            return Err(SimulationError::OutOfBlockGas);
                        }
                        // Executed but excluded: roll its state back.
                        vm.undo_last_tx().map_err(|e| SimulationError::Execution(e.to_string()))?;
                        include_tx[i] = false;
                        continue;
                    }
                    gas_used += tx_gas;
                    blobs_added += tx.blob_hashes.len() as u64;
                } else if can_be_dropped {
                    // Reverted and not allowed to: drop it instead.
                    vm.undo_last_tx().map_err(|e| SimulationError::Execution(e.to_string()))?;
                    include_tx[i] = false;
                } else {
                    return Err(SimulationError::RevertNotAllowed(i));
                }
            }
            Err(_) if can_be_dropped || can_revert => {
                // Likely invalidated by an earlier tx (e.g. nonce); drop it.
                include_tx[i] = false;
            }
            Err(_) => return Err(SimulationError::DropNotAllowed(i)),
        }
    }

    let final_balance = balance_of(vm, beneficiary)?;
    let builder_payment = au256(final_balance.saturating_sub(initial_balance));
    if builder_payment.is_zero() {
        return Err(SimulationError::ZeroBuilderPayment);
    }
    Ok(SimulatedOrder { order_ix, include_tx, builder_payment })
}

pub fn balance_of(
    vm: &mut Evm,
    address: ethrex_common::Address,
) -> Result<ethrex_common::U256, SimulationError> {
    vm.db
        .get_account(address)
        .map(|account| account.info.balance)
        .map_err(|e| SimulationError::Execution(e.to_string()))
}
