use ethrex_common::{
    Address, Bloom, H256, U256,
    types::{BlockHeader, Receipt, TxType, bloom_from_logs},
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::db::gen_db::{AccountSnapshot, TxReads};
use ethrex_rlp::encode::{RLPEncode, encode_length};

use crate::{
    merge::{
        bal::Fragment,
        pool::Range,
        state::{Account, BlockState, Checkpoint, KeyTable},
    },
    metrics,
};

pub const NO_EFFECT: u32 = u32::MAX;
const VARIANTS_PER_TX: usize = 16;
// Per builder and slot, 2.5-4x the most measured.
const MAX_EFFECTS: usize = 1 << 15;
const MAX_ACCOUNTS: usize = 1 << 17;
const MAX_SLOTS: usize = 1 << 18;
const MAX_LOGS: usize = 8 << 20;

/// A transaction's gas: `spent` is post-refund and cumulates into receipts; `regular`
/// and `state` are the EIP-8037 block dimensions, both pre-refund.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Gas {
    pub spent: u64,
    pub regular: u64,
    pub state: u64,
}

impl Gas {
    pub fn add(&mut self, gas: Gas) {
        self.spent += gas.spent;
        self.regular += gas.regular;
        self.state += gas.state;
    }

    /// The block's gas used: the larger dimension, which is `spent` before Amsterdam.
    pub fn block(&self) -> u64 {
        self.regular.max(self.state)
    }
}

pub type OwnReceipt = (Receipt, Bloom, Option<Fragment>);

pub struct EffectCache {
    latest_by_tx: Vec<u32>,
    entries: Vec<Effect>,
    receipts: Vec<(Receipt, Bloom)>,
    fragments: Vec<Option<Fragment>>,
    account_entries: Vec<(u32, AccountEntry)>,
    slot_entries: Vec<(u32, U256)>,
    logs_rlp: Vec<u8>,
    header_env: (Address, u64, u64, H256),
}

#[derive(Clone, Copy)]
struct Effect {
    tx: u32,
    prev: u32,
    accounts: Range,
    account_writes: u32,
    slots: Range,
    slot_writes: u32,
    logs: Range,
    coinbase_fee: U256,
    gas: Gas,
    loads_coinbase: bool,
}

#[derive(Clone, Copy)]
struct AccountEntry {
    account: Account,
    rule: BalanceRule,
}

#[derive(Clone, Copy, PartialEq)]
enum BalanceRule {
    Exact,
    AtLeast,
    Plus,
    Minus,
}

impl AccountEntry {
    fn matches_read(&self, now: &Account) -> bool {
        if self.rule == BalanceRule::Exact {
            return self.account == *now;
        }
        let read = &self.account;
        read.nonce == now.nonce &&
            read.code_hash == now.code_hash &&
            read.has_storage == now.has_storage &&
            now.balance >= read.balance &&
            !now.is_empty()
    }
}

impl Default for EffectCache {
    fn default() -> EffectCache {
        EffectCache {
            latest_by_tx: Vec::new(),
            entries: Vec::with_capacity(MAX_EFFECTS),
            receipts: Vec::with_capacity(MAX_EFFECTS),
            fragments: Vec::with_capacity(MAX_EFFECTS),
            account_entries: Vec::with_capacity(MAX_ACCOUNTS),
            slot_entries: Vec::with_capacity(MAX_SLOTS),
            logs_rlp: Vec::with_capacity(MAX_LOGS),
            header_env: Default::default(),
        }
    }
}

impl EffectCache {
    pub fn reset_for(&mut self, header: &BlockHeader) {
        let env = (header.coinbase, header.gas_limit, header.timestamp, header.prev_randao);
        if env != self.header_env {
            self.clear();
            self.header_env = env;
        }
    }

    pub fn clear(&mut self) {
        for effect in &self.entries {
            self.latest_by_tx[effect.tx as usize] = NO_EFFECT;
        }
        self.entries.clear();
        self.receipts.clear();
        self.fragments.clear();
        self.account_entries.clear();
        self.slot_entries.clear();
        self.logs_rlp.clear();
        self.header_env = Default::default();
    }

    pub fn find_replayable(
        &self,
        tx: u32,
        keys: &KeyTable,
        state: &BlockState,
        coinbase: u32,
    ) -> Option<u32> {
        let mut ix = *self.latest_by_tx.get(tx as usize)?;
        'variants: for _ in 0..VARIANTS_PER_TX {
            let effect = self.entries.get(ix as usize)?;
            let next = effect.prev;
            if effect.loads_coinbase && !state.account(keys, coinbase).is_plain_eoa() {
                ix = next;
                continue;
            }
            let (_, reads) = effect.accounts.split_at(effect.account_writes);
            for &(id, read) in reads.slice(&self.account_entries) {
                if !read.matches_read(&state.account(keys, id)) {
                    ix = next;
                    continue 'variants;
                }
            }
            let (_, reads) = effect.slots.split_at(effect.slot_writes);
            for &(id, value) in reads.slice(&self.slot_entries) {
                if state.slot(keys, id) != value {
                    ix = next;
                    continue 'variants;
                }
            }
            return Some(ix);
        }
        None
    }

    pub fn replay(
        &self,
        ix: u32,
        keys: &KeyTable,
        state: &mut BlockState,
        coinbase: u32,
    ) -> Option<Gas> {
        let effect = self.entries[ix as usize];
        let (writes, _) = effect.accounts.split_at(effect.account_writes);
        for &(id, write) in writes.slice(&self.account_entries) {
            let mut now = state.account(keys, id);
            now.balance = match write.rule {
                BalanceRule::Plus => now.balance.checked_add(write.account.balance)?,
                BalanceRule::Minus => now.balance.checked_sub(write.account.balance)?,
                BalanceRule::Exact | BalanceRule::AtLeast => write.account.balance,
            };
            now.nonce = write.account.nonce;
            now.code_hash = write.account.code_hash;
            now.has_storage = write.account.has_storage;
            if !state.set_account(id, now) {
                return None;
            }
        }
        let (slot_writes, _) = effect.slots.split_at(effect.slot_writes);
        for &(id, value) in slot_writes.slice(&self.slot_entries) {
            if !state.set_slot(id, value) {
                return None;
            }
        }
        if !effect.coinbase_fee.is_zero() {
            let mut now = state.account(keys, coinbase);
            now.balance = now.balance.checked_add(effect.coinbase_fee)?;
            if !state.set_account(coinbase, now) {
                return None;
            }
        }
        Some(effect.gas)
    }

    pub fn receipt(&self, ix: u32) -> (&Receipt, &Bloom, &[u8]) {
        let (receipt, bloom) = &self.receipts[ix as usize];
        (receipt, bloom, self.entries[ix as usize].logs.slice(&self.logs_rlp))
    }

    pub fn fragment(&self, ix: u32) -> Option<&Fragment> {
        self.fragments[ix as usize].as_ref()
    }

    #[expect(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        tx: u32,
        gas: Gas,
        keys: &mut KeyTable,
        state: &BlockState,
        at: Checkpoint,
        reads: &TxReads,
        coinbase: u32,
        before: U256,
        receipt: Receipt,
        fragment: Option<Fragment>,
        own: &mut Vec<OwnReceipt>,
    ) -> u32 {
        let bloom = bloom_from_logs(&receipt.logs, &NativeCrypto);
        // Writes kept are of keys read, so a tx adds at most twice its reads.
        let full = self.entries.len() == MAX_EFFECTS ||
            self.account_entries.len() + 2 * reads.accounts.len() > MAX_ACCOUNTS ||
            self.slot_entries.len() + 2 * reads.slots.len() > MAX_SLOTS ||
            self.logs_rlp.len() + receipt.logs.length() > MAX_LOGS;
        if full {
            metrics::rejection("merge_effects", "full");
        }
        if full || receipt.tx_type == TxType::Frame {
            own.push((receipt, bloom, fragment));
            return NO_EFFECT;
        }
        let (accounts, slots) = (self.account_entries.len(), self.slot_entries.len());
        let relaxed = |address: &Address, read: &AccountSnapshot| {
            !reads.balances.contains(address) && !Account::from_snapshot(read).is_empty()
        };
        let mut fee = U256::zero();
        let mut loads_coinbase = false;
        let (mut writes, mut slot_writes) = (0, 0);
        let replayable = 'push: {
            for id in state.accounts_since(at) {
                let (address, _) = keys.parent_account(id);
                let now = state.account(keys, id);
                let read = reads.accounts.get(&address);
                // A plain coinbase not inspected or sent from only receives fees and transfers.
                let unread = read.is_none_or(|read| {
                    let read = Account::from_snapshot(read);
                    !reads.balances.contains(&address) &&
                        read.is_plain_eoa() &&
                        read.nonce == now.nonce &&
                        read.code_hash == now.code_hash
                });
                if id == coinbase && unread {
                    let Some(credit) = now.balance.checked_sub(before) else { break 'push false };
                    fee = credit;
                    loads_coinbase = read.is_some();
                    continue;
                }
                let Some(read) = read else { break 'push false };
                let mut write = AccountEntry { account: now, rule: BalanceRule::Exact };
                if relaxed(&address, read) {
                    let first = read.info.balance;
                    (write.rule, write.account.balance) = match now.balance.checked_sub(first) {
                        Some(credit) => (BalanceRule::Plus, credit),
                        None => (BalanceRule::Minus, first - now.balance),
                    };
                }
                self.account_entries.push((id, write));
            }
            for id in state.slots_since(at) {
                let (address, key, _) = keys.parent_slot(id);
                if !reads.slots.contains_key(&(address, key)) {
                    break 'push false;
                }
                self.slot_entries.push((id, state.slot(keys, id)));
            }
            writes = (self.account_entries.len() - accounts) as u32;
            slot_writes = (self.slot_entries.len() - slots) as u32;
            for (address, snapshot) in &reads.accounts {
                let Ok(id) = keys.intern_account(*address) else { break 'push false };
                if loads_coinbase && id == coinbase {
                    continue;
                }
                let mut read = AccountEntry {
                    account: Account::from_snapshot(snapshot),
                    rule: BalanceRule::Exact,
                };
                if relaxed(address, snapshot) {
                    // A replay's debit cannot exceed what the floor guarantees.
                    let floor = reads.floors.iter().find(|(a, _)| a == address).map(|&(_, f)| f);
                    let debit =
                        read.account.balance.saturating_sub(state.account(keys, id).balance);
                    read.rule = BalanceRule::AtLeast;
                    read.account.balance = floor.unwrap_or_default().max(debit);
                }
                self.account_entries.push((id, read));
            }
            for (&(address, key), &value) in &reads.slots {
                let Ok(id) = keys.intern_slot(address, key) else { break 'push false };
                self.slot_entries.push((id, value));
            }
            true
        };
        if !replayable {
            self.account_entries.truncate(accounts);
            self.slot_entries.truncate(slots);
            own.push((receipt, bloom, fragment));
            return NO_EFFECT;
        }
        let logs = self.logs_rlp.len();
        receipt.logs.encode(&mut self.logs_rlp);
        let ix = self.entries.len() as u32;
        if self.latest_by_tx.len() <= tx as usize {
            self.latest_by_tx.resize(tx as usize + 1, NO_EFFECT);
        }
        let prev = self.latest_by_tx[tx as usize];
        self.latest_by_tx[tx as usize] = ix;
        self.entries.push(Effect {
            tx,
            prev,
            accounts: Range::between(accounts, self.account_entries.len()),
            account_writes: writes,
            slots: Range::between(slots, self.slot_entries.len()),
            slot_writes,
            logs: Range::between(logs, self.logs_rlp.len()),
            coinbase_fee: fee,
            gas,
            loads_coinbase,
        });
        self.receipts.push((receipt, bloom));
        self.fragments.push(fragment);
        ix
    }
}

pub fn put_receipt(out: &mut Vec<u8>, receipt: &Receipt, gas: u64, bloom: &Bloom, logs: &[u8]) {
    debug_assert!(receipt.tx_type != TxType::Frame);
    if receipt.tx_type != TxType::Legacy {
        out.push(receipt.tx_type as u8);
    }
    // Status, gas, then the bloom as a 256-byte string (0xb9 0x01 0x00).
    encode_length(1 + gas.length() + 3 + 256 + logs.len(), out);
    out.push(if receipt.succeeded { 0x01 } else { 0x80 });
    gas.encode(out);
    out.extend_from_slice(&[0xb9, 0x01, 0x00]);
    out.extend_from_slice(bloom.as_bytes());
    out.extend_from_slice(logs);
}
