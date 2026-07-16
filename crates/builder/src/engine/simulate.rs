//! Order presimulation on cloned EVM state. Port of the simulator's
//! `simulate_order` (`crates/simulator/src/block_merging/mod.rs:1013`) onto
//! ethrex: cloning `Evm` copies only the touched-account cache, so a clone per
//! candidate order gives the same isolation revm's `CacheDB` wrapper did.

use ethrex_common::types::BlockHeader;
use ethrex_vm::Evm;
use rustc_hash::FxHashSet;

use crate::engine::{
    convert::au256,
    error::SimulationError,
    types::{PreparedOrder, SimulatedOrder},
};

/// Cheap pre-checks shared by presim and live re-sim: duplicate txs and
/// aggregate gas/blob headroom.
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

/// Simulates an order on top of `vm` (a clone of the session's live EVM).
/// Returns gas used, per-tx inclusion flags and the beneficiary balance delta.
///
/// The clone accumulates state as txs apply, so bundle-internal dependencies
/// resolve exactly as they would on the live state.
pub fn simulate_order(
    vm: &mut Evm,
    header: &BlockHeader,
    order: &PreparedOrder,
    order_ix: usize,
    available_gas: u64,
    available_blobs: u64,
    beneficiary: ethrex_common::Address,
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

        match vm.execute_tx(&tx.tx, header, &mut cumulative_gas_spent, tx.sender) {
            Ok((receipt, report)) => {
                if receipt.succeeded || can_revert {
                    if gas_used + report.gas_used > available_gas {
                        if !can_be_dropped {
                            return Err(SimulationError::OutOfBlockGas);
                        }
                        // Executed but excluded: roll its state back.
                        vm.undo_last_tx().map_err(|e| SimulationError::Execution(e.to_string()))?;
                        include_tx[i] = false;
                        continue;
                    }
                    gas_used += report.gas_used;
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
