use std::sync::Arc;

use alloy_primitives::B256;
use dashmap::DashMap;
use ethrex_blockchain::{
    Blockchain,
    error::ChainError,
    payload::{HeadTransaction, PayloadBuildContext},
};
use ethrex_common::{
    Address, H256, U256,
    constants::GAS_PER_BLOB,
    types::{BlockHeader, Code, Log, Receipt},
};
use ethrex_levm::{
    account::AccountStatus,
    call_frame::CallFrameBackup,
    db::gen_db::{AccountSnapshot, GeneralizedDatabase, TxReads},
    errors::InternalError,
};
use ethrex_vm::{Evm, EvmError};
use rustc_hash::FxHashMap;

use crate::{engine::types::DecodedTx, metrics};

/// Each builder's tx results this slot, keyed by builder and tx hash, newest first. A tx sees
/// a few distinct states over a slot (base replay, presim, order apply) and switches between
/// them, so one result per tx would keep evicting the one about to be needed again.
pub type Cache = Arc<DashMap<(alloy_primitives::Address, B256), Vec<Arc<TxResult>>>>;

const VARIANTS: usize = 4;

/// What a tx moved in the payload context: block gas, receipt gas, block value, blob gas.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    remaining_gas: u64,
    cumulative_gas: u64,
    block_value: U256,
    blob_gas: u64,
}

impl Totals {
    pub fn of(ctx: &PayloadBuildContext) -> Self {
        Self {
            remaining_gas: ctx.remaining_gas,
            cumulative_gas: ctx.cumulative_gas_spent,
            block_value: ctx.block_value,
            blob_gas: ctx.payload.header.blob_gas_used.unwrap_or_default(),
        }
    }

    fn since(self, before: Self) -> Option<Self> {
        Some(Self {
            remaining_gas: before.remaining_gas.checked_sub(self.remaining_gas)?,
            cumulative_gas: self.cumulative_gas.checked_sub(before.cumulative_gas)?,
            block_value: self.block_value.checked_sub(before.block_value)?,
            blob_gas: self.blob_gas.checked_sub(before.blob_gas)?,
        })
    }
}

/// One executed tx: what it read as it first found it, what it changed, and its receipt.
pub struct TxResult {
    reads: TxReads,
    accounts: FxHashMap<Address, AccountSnapshot>,
    slots: FxHashMap<(Address, H256), U256>,
    codes: Vec<Code>,
    coinbase_credit: U256,
    moved: Totals,
    succeeded: bool,
    logs: Vec<Log>,
}

/// Ignores `status`: it tracks whether anything in the block touched the account, not its state.
fn same_state(a: &AccountSnapshot, b: &AccountSnapshot) -> bool {
    a.info == b.info && a.has_storage == b.has_storage && a.exists == b.exists
}

fn destroyed(status: &AccountStatus) -> bool {
    matches!(status, AccountStatus::Destroyed | AccountStatus::DestroyedModified)
}

impl TxResult {
    /// Takes the result of the tx that just ran on `db` with `tx_reads` on. `None` when it read
    /// the coinbase or touched a destroyed account.
    fn capture(
        db: &mut GeneralizedDatabase,
        coinbase: Address,
        coinbase_before: U256,
        moved: Totals,
        succeeded: bool,
        logs: Vec<Log>,
    ) -> Option<Self> {
        let reads = db.tx_reads.take()?;
        if reads.accounts.contains_key(&coinbase) {
            return None;
        }
        let mut accounts = FxHashMap::default();
        let mut codes = Vec::new();
        for (address, before) in &reads.accounts {
            let after = db.current_accounts_state.get(address)?;
            if destroyed(&before.status) || destroyed(&after.status) {
                return None;
            }
            let after = AccountSnapshot::of(after);
            if same_state(&after, before) {
                continue;
            }
            if after.info.code_hash != before.info.code_hash {
                codes.push(db.codes.get(&after.info.code_hash)?.clone());
            }
            accounts.insert(*address, after);
        }
        let mut slots = FxHashMap::default();
        for (&(address, key), before) in &reads.slots {
            let after = *db.current_accounts_state.get(&address)?.storage.get(&key)?;
            if after != *before {
                slots.insert((address, key), after);
            }
        }
        let coinbase_after = coinbase_balance(db, coinbase).ok()?;
        Some(Self {
            reads,
            accounts,
            slots,
            codes,
            coinbase_credit: coinbase_after.checked_sub(coinbase_before)?,
            moved,
            succeeded,
            logs,
        })
    }

    fn effect(&self, hash: B256, coinbase: Address) -> crate::engine::incremental::Effect {
        use crate::engine::incremental::{Effect, Key, Val};
        let mut effect = Effect {
            hash,
            reads: self
                .reads
                .accounts
                .iter()
                .map(|(a, s)| (Key::Account(*a), Val::of_snapshot(s)))
                .chain(self.reads.slots.iter().map(|(&(a, k), v)| (Key::Slot(a, k), Val::Slot(*v))))
                .collect(),
            writes: self
                .accounts
                .iter()
                .map(|(a, s)| (Key::Account(*a), Val::of_snapshot(s)))
                .chain(self.slots.iter().map(|(&(a, k), v)| (Key::Slot(a, k), Val::Slot(*v))))
                .collect(),
            reads_coinbase: false,
            credit: self.coinbase_credit,
            debit: U256::zero(),
            coinbase_nonce: None,
            coinbase_seen: None,
            coinbase,
            coinbase_address: self.reads.coinbase_address,
            codes: self.codes.clone(),
        };
        effect.sort();
        effect
    }

    /// Whether `db` still holds every value the tx read, so running it there gives this result.
    fn holds(&self, db: &mut GeneralizedDatabase) -> Result<bool, InternalError> {
        for (address, before) in &self.reads.accounts {
            let account = db.get_account(*address)?;
            if account.info != before.info ||
                account.has_storage != before.has_storage ||
                account.exists != before.exists ||
                destroyed(&account.status)
            {
                return Ok(false);
            }
        }
        for (&(address, key), before) in &self.reads.slots {
            if db.storage_value(address, key)? != *before {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Writes the tx's changes to `db`, which [`Self::holds`] its reads, and returns the backup
    /// that undoes them.
    fn apply_state(
        &self,
        db: &mut GeneralizedDatabase,
        coinbase: Address,
    ) -> Result<CallFrameBackup, InternalError> {
        let mut backup = CallFrameBackup::default();
        let mut save =
            |db: &mut GeneralizedDatabase, address: Address| -> Result<(), InternalError> {
                if !backup.original_accounts_info.contains_key(&address) {
                    let account = db.get_account(address)?.clone_without_storage();
                    backup.original_accounts_info.insert(address, account);
                }
                Ok(())
            };
        for (address, after) in &self.accounts {
            save(db, *address)?;
            let account = db.get_account_mut(*address)?;
            account.info = after.info.clone();
            account.has_storage = after.has_storage;
            account.exists = after.exists;
        }
        save(db, coinbase)?;
        let account = db.get_account_mut(coinbase)?;
        account.info.balance = account
            .info
            .balance
            .checked_add(self.coinbase_credit)
            .ok_or(InternalError::Overflow)?;
        for (&(address, key), value) in &self.slots {
            let old = db.storage_value(address, key)?;
            backup
                .original_account_storage_slots
                .entry(address)
                .or_default()
                .entry(key)
                .or_insert(old);
            db.get_account_mut(address)?.storage.insert(key, *value);
        }
        for code in &self.codes {
            if !db.codes.contains_key(&code.hash) {
                db.codes.insert(code.hash, code.clone());
                backup.inserted_code_hashes.push(code.hash);
            }
        }
        Ok(backup)
    }

    fn same_outcome(&self, other: &Self) -> bool {
        self.accounts.len() == other.accounts.len() &&
            self.accounts
                .iter()
                .all(|(a, s)| other.accounts.get(a).is_some_and(|o| same_state(s, o))) &&
            self.slots == other.slots &&
            self.codes.iter().map(|c| c.hash).eq(other.codes.iter().map(|c| c.hash)) &&
            self.coinbase_credit == other.coinbase_credit &&
            self.moved == other.moved &&
            self.succeeded == other.succeeded &&
            self.logs == other.logs
    }
}

/// The coinbase balance, read without counting as the tx's own read.
fn coinbase_balance(
    db: &mut GeneralizedDatabase,
    coinbase: Address,
) -> Result<U256, InternalError> {
    let paused = std::mem::replace(&mut db.reads_paused, true);
    let balance = db.get_account(coinbase).map(|account| account.info.balance);
    db.reads_paused = paused;
    balance
}

/// A cached result for `hash` whose reads `db` still holds.
fn held(
    cache: &Cache,
    builder: alloy_primitives::Address,
    hash: B256,
    db: &mut GeneralizedDatabase,
) -> Result<(Option<Arc<TxResult>>, bool), InternalError> {
    let Some(variants) = cache.get(&(builder, hash)).map(|entry| entry.clone()) else {
        return Ok((None, false));
    };
    for result in &variants {
        if result.holds(db)? {
            return Ok((Some(result.clone()), true));
        }
    }
    Ok((variants.first().cloned(), false))
}

fn record(
    stage: &str,
    cache: &Cache,
    key: (alloy_primitives::Address, B256),
    cached: Option<Arc<TxResult>>,
    held: bool,
    fresh: Option<TxResult>,
) {
    metrics::tx_reuse(stage, match (held, &cached, &fresh) {
        (false, None, _) => "no_entry",
        (false, ..) => "not_held",
        (true, Some(old), Some(new)) if old.same_outcome(new) => "verified",
        (true, ..) => {
            tracing::warn!(stage, tx = %key.1, "reused result differs");
            "mismatch"
        }
    });
    if let Some(fresh) = fresh {
        let mut variants = cache.entry(key).or_default();
        variants.insert(0, Arc::new(fresh));
        variants.truncate(VARIANTS);
    }
}

pub enum RunError {
    Tx(ChainError),
    Internal(InternalError),
}

/// Adds `head` to the payload: from a cached result when its reads still hold, else by running
/// it and caching the result. Leaves the tx's undo backup in `ctx.vm.db.tx_backup` either way.
#[allow(clippy::too_many_arguments)]
pub fn apply_tx(
    stage: &str,
    cache: &Cache,
    verify: bool,
    blockchain: &Blockchain,
    ctx: &mut PayloadBuildContext,
    head: HeadTransaction,
    hash: B256,
    builder: alloy_primitives::Address,
    coinbase: Address,
) -> Result<(), RunError> {
    let (cached, held) = held(cache, builder, hash, &mut ctx.vm.db).map_err(RunError::Internal)?;
    if let (Some(result), true, false) = (&cached, held, verify) {
        let backup = result.apply_state(&mut ctx.vm.db, coinbase).map_err(RunError::Internal)?;
        ctx.vm.db.tx_backup = Some(backup);
        ctx.remaining_gas -= result.moved.remaining_gas;
        ctx.cumulative_gas_spent += result.moved.cumulative_gas;
        ctx.block_value += result.moved.block_value;
        if result.moved.blob_gas > 0 {
            ctx.payload.header.blob_gas_used =
                Some(ctx.payload.header.blob_gas_used.unwrap_or_default() + result.moved.blob_gas);
        }
        ctx.receipts.push(Receipt::new(
            head.tx.tx_type(),
            result.succeeded,
            ctx.cumulative_gas_spent,
            result.logs.clone(),
        ));
        ctx.payload.body.transactions.push(head.into());
        metrics::tx_reuse(stage, "applied");
        if crate::engine::incremental::recording() {
            crate::engine::incremental::A_REUSED.with(|r| r.borrow_mut().push(true));
            crate::engine::incremental::push(Some(Arc::new(result.effect(hash, coinbase))));
        }
        return Ok(());
    }
    let coinbase_before = coinbase_balance(&mut ctx.vm.db, coinbase).map_err(RunError::Internal)?;
    let before = Totals::of(ctx);
    ctx.vm.db.tx_reads = Some(Default::default());
    let applied = blockchain.apply_tx_to_payload(head, ctx);
    if let Err(err) = applied {
        ctx.vm.db.tx_reads = None;
        return Err(RunError::Tx(err));
    }
    if crate::engine::incremental::shadow_enabled() {
        let coinbase_after =
            coinbase_balance(&mut ctx.vm.db, coinbase).map_err(RunError::Internal)?;
        let effect = ctx.vm.db.tx_reads.as_ref().and_then(|reads| {
            crate::engine::incremental::Effect::from_db(
                hash,
                &ctx.vm.db,
                reads,
                coinbase,
                (
                    coinbase_after.saturating_sub(coinbase_before),
                    coinbase_before.saturating_sub(coinbase_after),
                ),
            )
        });
        if let Some(effect) = &effect {
            if stage != "base" {
                let effect = effect.clone();
                crate::engine::incremental::defer(move || {
                    crate::engine::incremental::learn(effect)
                });
            }
        }
        if crate::engine::incremental::recording() {
            crate::engine::incremental::A_EXECUTED.with(|c| c.set(c.get() + 1));
            crate::engine::incremental::A_REUSED.with(|r| r.borrow_mut().push(false));
            crate::engine::incremental::push(effect.map(Arc::new));
        }
    }
    let fresh = match (Totals::of(ctx).since(before), ctx.receipts.last()) {
        (Some(moved), Some(receipt)) => {
            let (succeeded, logs) = (receipt.succeeded, receipt.logs.clone());
            TxResult::capture(&mut ctx.vm.db, coinbase, coinbase_before, moved, succeeded, logs)
        }
        _ => None,
    };
    ctx.vm.db.tx_reads = None;
    record(stage, cache, (builder, hash), cached, held, fresh);
    Ok(())
}

/// Presim's `execute_tx`, from a cached result when its reads still hold. Returns whether the
/// tx succeeded and the block gas it used.
#[allow(clippy::too_many_arguments)]
pub fn presim_tx(
    cache: &Cache,
    verify: bool,
    vm: &mut Evm,
    header: &BlockHeader,
    tx: &DecodedTx,
    cumulative_gas_spent: &mut u64,
    builder: alloy_primitives::Address,
) -> Result<(bool, u64), EvmError> {
    let internal = |e: InternalError| EvmError::Custom(e.to_string());
    let coinbase = header.coinbase;
    let (cached, held) = held(cache, builder, tx.hash, &mut vm.db).map_err(internal)?;
    if let (Some(result), true, false) = (&cached, held, verify) {
        let backup = result.apply_state(&mut vm.db, coinbase).map_err(internal)?;
        vm.db.tx_backup = Some(backup);
        *cumulative_gas_spent += result.moved.cumulative_gas;
        metrics::tx_reuse("presim", "applied");
        return Ok((result.succeeded, result.moved.remaining_gas));
    }
    let coinbase_before = coinbase_balance(&mut vm.db, coinbase).map_err(internal)?;
    vm.db.tx_reads = Some(Default::default());
    let executed = vm.execute_tx(&tx.tx, header, cumulative_gas_spent, tx.sender);
    let (receipt, report) = match executed {
        Ok(executed) => executed,
        Err(err) => {
            vm.db.tx_reads = None;
            return Err(err);
        }
    };
    let tip = tx.tx.effective_gas_tip(header.base_fee_per_gas).unwrap_or_default();
    let moved = Totals {
        remaining_gas: report.gas_used,
        cumulative_gas: report.gas_spent,
        block_value: U256::from(report.gas_spent) * tip,
        blob_gas: tx.blob_hashes.len() as u64 * u64::from(GAS_PER_BLOB),
    };
    if crate::engine::incremental::shadow_enabled() {
        let coinbase_after = coinbase_balance(&mut vm.db, coinbase).map_err(internal)?;
        if let Some(effect) = vm.db.tx_reads.as_ref().and_then(|reads| {
            crate::engine::incremental::Effect::from_db(
                tx.hash,
                &vm.db,
                reads,
                coinbase,
                (
                    coinbase_after.saturating_sub(coinbase_before),
                    coinbase_before.saturating_sub(coinbase_after),
                ),
            )
        }) {
            crate::engine::incremental::defer(move || crate::engine::incremental::learn(effect));
        }
    }
    let fresh = TxResult::capture(
        &mut vm.db,
        coinbase,
        coinbase_before,
        moved,
        receipt.succeeded,
        receipt.logs,
    );
    vm.db.tx_reads = None;
    record("presim", cache, (builder, tx.hash), cached, held, fresh);
    Ok((receipt.succeeded, report.gas_used))
}
