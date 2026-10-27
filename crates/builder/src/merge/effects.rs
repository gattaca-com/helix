//! A builder's txs, from its bases and the orders run on them, kept as effects: what each read, as
//! it first found it, and what it wrote, by `Keys` id. A later point of any of the builder's blocks
//! replays a tx's effect when every value it read is the same there (a balance it never inspected
//! need only pass the same value checks, see `Rule`), and runs only the others. Reads are the
//! fork's `TxReads`; writes are what `State::fold` wrote for the run. Code a run deploys stays in
//! `Keys`. A tx keeps its last `VARIANTS` effects, newest first, for the builder's (one
//! coinbase's) blocks in the slot, while the header fields a tx can observe stay the same.
//!
//! Not recorded, so always run: a tx that wrote a key it never read, a `TxType::Frame` tx, and any
//! tx once the store's fixed capacity is full.
//!
//! Each effect keeps its receipt, its logs bloom and its logs encoded, so a replay neither hashes
//! nor encodes its logs again: `put_receipt` encodes the receipt from them at the replaying block's
//! cumulative gas. Effects are never moved; one past its tx's last `VARIANTS` is dead weight.

use ethrex_common::{
    Address, Bloom, H256, U256,
    types::{BlockHeader, Receipt, TxType, bloom_from_logs},
};
use ethrex_crypto::native::NativeCrypto;
use ethrex_levm::db::gen_db::{AccountSnapshot, TxReads};
use ethrex_rlp::encode::{RLPEncode, encode_length};

use crate::{
    merge::{
        ingest::Range,
        state::{Acct, At, Keys, State},
    },
    metrics,
};

pub const NONE: u32 = u32::MAX;
/// Effects kept per tx: its state at a point tends to flip between a few versions across bases.
const VARIANTS: usize = 16;
/// Per builder and slot, 2.5-4x the most measured: 10.8k effects, 36k account and 106k slot
/// entries, 2.2 MB of logs.
const MAX_EFFECTS: usize = 1 << 15;
const MAX_ACCOUNTS: usize = 1 << 17;
const MAX_SLOTS: usize = 1 << 18;
const MAX_LOGS: usize = 8 << 20;

pub struct Effects {
    /// By pool tx index: its latest effect, or `NONE`. Older ones follow `Effect::prev`.
    at: Vec<u32>,
    txs: Vec<Effect>,
    /// By effect: its receipt, whose cumulative gas is the recording block's, and logs bloom.
    receipts: Vec<(Receipt, Bloom)>,
    /// Each effect's account writes, then its reads, by account id.
    accounts: Vec<(u32, Account)>,
    /// Each effect's slot writes, then its reads, by slot id.
    slots: Vec<(u32, U256)>,
    /// Each effect's receipt logs, RLP encoded.
    logs: Vec<u8>,
    /// Header fields a tx can read that the parent does not fix: coinbase, gas limit, timestamp,
    /// randao.
    env: (Address, u64, u64, H256),
}

#[derive(Clone, Copy)]
struct Effect {
    tx: u32,
    /// The tx's previous effect, or `NONE`; always an earlier index.
    prev: u32,
    accounts: Range,
    /// The first this many of `accounts` are writes.
    writes: u32,
    slots: Range,
    slot_writes: u32,
    logs: Range,
    /// Credited to the coinbase by a tx that did not read its balance; one that did writes it
    /// absolutely.
    fee: U256,
    gas: u64,
    /// The tx loaded the coinbase but not its balance: it holds while the coinbase is `plain`.
    coinbase: bool,
}

#[derive(Clone, Copy)]
struct Account {
    acct: Acct,
    rule: Rule,
}

/// How an entry's `balance` applies. A read is `Exact` or `AtLeast` a floor; a write sets it
/// (`Exact`) or moves it by `Plus` or `Minus`. A tx that never inspected an account's balance
/// sees it only through the value checks behind each debit (`TxReads::floors`) and whether the
/// account is empty, so it runs the same at any balance that keeps both.
#[derive(Clone, Copy, PartialEq)]
enum Rule {
    Exact,
    AtLeast,
    Plus,
    Minus,
}

impl Account {
    /// A read entry: `now` gives the tx the same run.
    fn admits(&self, now: &Acct) -> bool {
        if self.rule == Rule::Exact {
            return self.acct == *now;
        }
        let read = &self.acct;
        read.nonce == now.nonce &&
            read.code_hash == now.code_hash &&
            read.has_storage == now.has_storage &&
            now.balance >= read.balance &&
            !now.is_empty()
    }
}

impl Default for Effects {
    fn default() -> Effects {
        Effects {
            at: Vec::new(),
            txs: Vec::with_capacity(MAX_EFFECTS),
            receipts: Vec::with_capacity(MAX_EFFECTS),
            accounts: Vec::with_capacity(MAX_ACCOUNTS),
            slots: Vec::with_capacity(MAX_SLOTS),
            logs: Vec::with_capacity(MAX_LOGS),
            env: Default::default(),
        }
    }
}

impl Effects {
    /// Readies the store for a base under `header`: other header fields a tx can read drop every
    /// effect.
    pub fn open(&mut self, header: &BlockHeader) {
        let env = (header.coinbase, header.gas_limit, header.timestamp, header.prev_randao);
        if env == self.env {
            return;
        }
        for effect in &self.txs {
            self.at[effect.tx as usize] = NONE;
        }
        self.txs.clear();
        self.receipts.clear();
        self.accounts.clear();
        self.slots.clear();
        self.logs.clear();
        self.env = env;
    }

    /// The newest of pool tx `tx`'s last `VARIANTS` effects whose every read value is the same in
    /// `state`; `coinbase` is the coinbase's account id. None holds once its tx is in the block:
    /// that changed the sender's nonce every effect reads.
    pub fn holds(&self, tx: u32, keys: &Keys, state: &State, coinbase: u32) -> Option<u32> {
        let mut ix = *self.at.get(tx as usize)?;
        'variants: for _ in 0..VARIANTS {
            let effect = self.txs.get(ix as usize)?;
            let next = effect.prev;
            if effect.coinbase && !state.account(keys, coinbase).plain() {
                ix = next;
                continue;
            }
            let (_, reads) = effect.accounts.split(effect.writes);
            for &(id, read) in reads.of(&self.accounts) {
                if !read.admits(&state.account(keys, id)) {
                    ix = next;
                    continue 'variants;
                }
            }
            let (_, reads) = effect.slots.split(effect.slot_writes);
            for &(id, value) in reads.of(&self.slots) {
                if state.slot(keys, id) != value {
                    ix = next;
                    continue 'variants;
                }
            }
            return Some(ix);
        }
        None
    }

    /// Writes effect `ix`, which `holds` returned, to `state`. Returns the tx's gas. On `None`,
    /// part may be written; the caller undoes it.
    pub fn replay(&self, ix: u32, keys: &Keys, state: &mut State, coinbase: u32) -> Option<u64> {
        let effect = self.txs[ix as usize];
        let (writes, _) = effect.accounts.split(effect.writes);
        for &(id, write) in writes.of(&self.accounts) {
            let mut now = state.account(keys, id);
            now.balance = match write.rule {
                Rule::Plus => now.balance.checked_add(write.acct.balance)?,
                Rule::Minus => now.balance.checked_sub(write.acct.balance)?,
                Rule::Exact | Rule::AtLeast => write.acct.balance,
            };
            now.nonce = write.acct.nonce;
            now.code_hash = write.acct.code_hash;
            now.has_storage = write.acct.has_storage;
            if !state.set_account(id, now) {
                return None;
            }
        }
        let (slot_writes, _) = effect.slots.split(effect.slot_writes);
        for &(id, value) in slot_writes.of(&self.slots) {
            if !state.set_slot(id, value) {
                return None;
            }
        }
        if !effect.fee.is_zero() {
            let mut now = state.account(keys, coinbase);
            now.balance = now.balance.checked_add(effect.fee)?;
            if !state.set_account(coinbase, now) {
                return None;
            }
        }
        Some(effect.gas)
    }

    /// Effect `ix`'s receipt as recorded, its logs bloom and its logs encoded.
    pub fn receipt(&self, ix: u32) -> (&Receipt, &Bloom, &[u8]) {
        let (receipt, bloom) = &self.receipts[ix as usize];
        (receipt, bloom, self.txs[ix as usize].logs.of(&self.logs))
    }

    /// Keeps pool tx `tx`'s effect from a run that read `reads` and wrote what `state` holds
    /// written since `at`, with its receipt. The coinbase, account id `coinbase`, held `before`
    /// just ahead of the run. Returns the effect, or `NONE` with the receipt and its bloom pushed
    /// to `own` when not recorded.
    #[expect(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        tx: u32,
        gas: u64,
        keys: &mut Keys,
        state: &State,
        at: At,
        reads: &TxReads,
        coinbase: u32,
        before: U256,
        receipt: Receipt,
        own: &mut Vec<(Receipt, Bloom)>,
    ) -> u32 {
        let bloom = bloom_from_logs(&receipt.logs, &NativeCrypto);
        // Writes kept are of keys read, so a tx adds at most twice its reads.
        let full = self.txs.len() == MAX_EFFECTS ||
            self.accounts.len() + 2 * reads.accounts.len() > MAX_ACCOUNTS ||
            self.slots.len() + 2 * reads.slots.len() > MAX_SLOTS ||
            self.logs.len() + receipt.logs.length() > MAX_LOGS;
        if full {
            metrics::rejection("merge_effects", "full");
        }
        if full || receipt.tx_type == TxType::Frame {
            own.push((receipt, bloom));
            return NONE;
        }
        let (accounts, slots) = (self.accounts.len(), self.slots.len());
        // Balance seen only through value checks, by an account non-empty when read.
        let relaxed = |address: &Address, read: &AccountSnapshot| {
            !reads.balances.contains(address) && !Acct::of_snapshot(read).is_empty()
        };
        let mut fee = U256::zero();
        let mut loads_coinbase = false;
        let (mut writes, mut slot_writes) = (0, 0);
        let replayable = 'push: {
            for id in state.accounts_since(at) {
                let (address, _) = keys.account_at(id);
                let now = state.account(keys, id);
                let read = reads.accounts.get(&address);
                // Unless the tx inspected its balance or sent from it, only fees and transfers
                // reach the coinbase, and a plain one behaves the same at any balance.
                let unread = read.is_none_or(|read| {
                    let read = Acct::of_snapshot(read);
                    !reads.balances.contains(&address) &&
                        read.plain() &&
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
                let mut write = Account { acct: now, rule: Rule::Exact };
                if relaxed(&address, read) {
                    let first = read.info.balance;
                    (write.rule, write.acct.balance) = match now.balance.checked_sub(first) {
                        Some(credit) => (Rule::Plus, credit),
                        None => (Rule::Minus, first - now.balance),
                    };
                }
                self.accounts.push((id, write));
            }
            for id in state.slots_since(at) {
                let (address, key, _) = keys.slot_at(id);
                if !reads.slots.contains_key(&(address, key)) {
                    break 'push false;
                }
                self.slots.push((id, state.slot(keys, id)));
            }
            writes = (self.accounts.len() - accounts) as u32;
            slot_writes = (self.slots.len() - slots) as u32;
            for (address, snapshot) in &reads.accounts {
                let Ok(id) = keys.account(*address) else { break 'push false };
                if loads_coinbase && id == coinbase {
                    continue;
                }
                let mut read = Account { acct: Acct::of_snapshot(snapshot), rule: Rule::Exact };
                if relaxed(address, snapshot) {
                    // A replay's debit cannot exceed what the floor guarantees.
                    let floor = reads.floors.iter().find(|(a, _)| a == address).map(|&(_, f)| f);
                    let debit = read.acct.balance.saturating_sub(state.account(keys, id).balance);
                    read.rule = Rule::AtLeast;
                    read.acct.balance = floor.unwrap_or_default().max(debit);
                }
                self.accounts.push((id, read));
            }
            for (&(address, key), &value) in &reads.slots {
                let Ok(id) = keys.slot(address, key) else { break 'push false };
                self.slots.push((id, value));
            }
            true
        };
        if !replayable {
            self.accounts.truncate(accounts);
            self.slots.truncate(slots);
            own.push((receipt, bloom));
            return NONE;
        }
        let logs = self.logs.len();
        receipt.logs.encode(&mut self.logs);
        let ix = self.txs.len() as u32;
        if self.at.len() <= tx as usize {
            self.at.resize(tx as usize + 1, NONE);
        }
        let prev = self.at[tx as usize];
        self.at[tx as usize] = ix;
        self.txs.push(Effect {
            tx,
            prev,
            accounts: Range::to(accounts, self.accounts.len()),
            writes,
            slots: Range::to(slots, self.slots.len()),
            slot_writes,
            logs: Range::to(logs, self.logs.len()),
            fee,
            gas,
            coinbase: loads_coinbase,
        });
        self.receipts.push((receipt, bloom));
        ix
    }
}

/// Appends `receipt` at cumulative gas `gas` as `Receipt::encode_inner_with_precomputed_bloom`
/// encodes it, given its logs already encoded. Not for `TxType::Frame`, whose receipts carry no
/// logs field.
pub fn put_receipt(out: &mut Vec<u8>, receipt: &Receipt, gas: u64, bloom: &Bloom, logs: &[u8]) {
    debug_assert!(receipt.tx_type != TxType::Frame);
    if receipt.tx_type != TxType::Legacy {
        out.push(receipt.tx_type as u8);
    }
    // Status byte, gas, then the bloom as a 256-byte string: 0xb9 0x01 0x00 and its bytes.
    encode_length(1 + gas.length() + 3 + 256 + logs.len(), out);
    out.push(if receipt.succeeded { 0x01 } else { 0x80 });
    gas.encode(out);
    out.extend_from_slice(&[0xb9, 0x01, 0x00]);
    out.extend_from_slice(bloom.as_bytes());
    out.extend_from_slice(logs);
}

#[cfg(test)]
mod tests {
    use ethrex_common::{
        Address, Bytes, H256,
        types::{Log, Receipt, TxType, bloom_from_logs},
    };
    use ethrex_crypto::native::NativeCrypto;
    use ethrex_rlp::encode::RLPEncode;

    use super::put_receipt;

    #[test]
    fn put_receipt_matches_ethrex() {
        let log = |n: u8, data: usize| Log {
            address: Address::repeat_byte(n),
            topics: (0..n % 5).map(|t| H256::repeat_byte(t)).collect(),
            data: Bytes::from(vec![n; data]),
        };
        let logs = [vec![], vec![log(1, 0)], vec![log(2, 31), log(3, 300), log(4, 70_000)]];
        let types =
            [TxType::Legacy, TxType::EIP2930, TxType::EIP1559, TxType::EIP4844, TxType::EIP7702];
        for (tx_type, logs) in types.into_iter().flat_map(|t| logs.iter().map(move |l| (t, l))) {
            for (succeeded, gas) in
                [(true, 0), (false, 1), (true, 127), (true, 128), (false, 29_999_999)]
            {
                let receipt = Receipt::new(tx_type, succeeded, gas, logs.clone());
                let bloom = bloom_from_logs(&receipt.logs, &NativeCrypto);
                let mut out = Vec::new();
                let logs = receipt.logs.encode_to_vec();
                put_receipt(&mut out, &receipt, receipt.cumulative_gas_used, &bloom, &logs);
                assert_eq!(out, receipt.encode_inner_with_precomputed_bloom(bloom));
            }
        }
    }
}
