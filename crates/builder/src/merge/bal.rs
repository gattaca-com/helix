use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use ethrex_common::{
    Address, U256,
    types::block_access_list::{
        AccountChanges, BalanceChange, BlockAccessList, CodeChange, NonceChange, SlotChange,
        StorageChange,
    },
};
use rustc_hash::FxHashMap;

/// One run's EIP-7928 accesses. Balances are kept as the run's own deltas: a replayed
/// effect runs the same from any pre-balance its reads allow, so its post-balances are
/// recovered from the balance the block has at that point.
#[derive(Default)]
pub struct Fragment {
    accounts: Vec<FragmentAccount>,
}

struct FragmentAccount {
    address: Address,
    writes: Vec<(U256, U256)>,
    reads: Vec<U256>,
    // (credit, amount)
    balance: Option<(bool, U256)>,
    nonce: Option<u64>,
    code: Option<Bytes>,
}

impl Fragment {
    pub fn from_run(bal: BlockAccessList, pre_balance: impl Fn(Address) -> U256) -> Fragment {
        let accounts = bal
            .accounts()
            .iter()
            .map(|account| {
                let last = |slot: &SlotChange| slot.slot_changes.last().map(|c| c.post_value);
                let balance = account.balance_changes.last().map(|change| {
                    let pre = pre_balance(account.address);
                    let post = change.post_balance;
                    if post >= pre { (true, post - pre) } else { (false, pre - post) }
                });
                FragmentAccount {
                    address: account.address,
                    writes: account
                        .storage_changes
                        .iter()
                        .filter_map(|slot| last(slot).map(|post| (slot.slot, post)))
                        .collect(),
                    reads: account.storage_reads.clone(),
                    balance,
                    nonce: account.nonce_changes.last().map(|c| c.post_nonce),
                    code: account.code_changes.last().map(|c| c.new_code.clone()),
                }
            })
            .collect();
        Fragment { accounts }
    }
}

#[derive(Default)]
struct Entry {
    writes: BTreeMap<U256, Vec<StorageChange>>,
    reads: BTreeSet<U256>,
    balances: Vec<BalanceChange>,
    nonces: Vec<NonceChange>,
    codes: Vec<CodeChange>,
}

/// Fragments in block order with their EIP-7928 indices; `parent_balance` is the
/// balance before the block.
pub fn assemble<'a>(
    fragments: impl Iterator<Item = (u32, &'a Fragment)>,
    parent_balance: impl Fn(Address) -> U256,
) -> BlockAccessList {
    let mut entries: BTreeMap<Address, Entry> = BTreeMap::new();
    let mut balances: FxHashMap<Address, U256> = FxHashMap::default();
    for (index, fragment) in fragments {
        for account in &fragment.accounts {
            let entry = entries.entry(account.address).or_default();
            for &(slot, post) in &account.writes {
                entry.writes.entry(slot).or_default().push(StorageChange::new(index, post));
            }
            entry.reads.extend(account.reads.iter().copied());
            if let Some((credit, amount)) = account.balance {
                let balance = balances
                    .entry(account.address)
                    .or_insert_with(|| parent_balance(account.address));
                *balance = match credit {
                    true => balance.saturating_add(amount),
                    false => balance.saturating_sub(amount),
                };
                entry.balances.push(BalanceChange::new(index, *balance));
            }
            if let Some(nonce) = account.nonce {
                entry.nonces.push(NonceChange::new(index, nonce));
            }
            if let Some(code) = &account.code {
                entry.codes.push(CodeChange::new(index, code.clone()));
            }
        }
    }
    let accounts = entries
        .into_iter()
        .map(|(address, entry)| {
            let reads = entry.reads.iter().filter(|slot| !entry.writes.contains_key(slot));
            let writes = entry
                .writes
                .iter()
                .map(|(&slot, changes)| SlotChange::with_changes(slot, changes.clone()))
                .collect();
            AccountChanges::new(address)
                .with_storage_changes(writes)
                .with_storage_reads(reads.copied().collect())
                .with_balance_changes(entry.balances)
                .with_nonce_changes(entry.nonces)
                .with_code_changes(entry.codes)
        })
        .collect();
    BlockAccessList::from_accounts(accounts)
}
