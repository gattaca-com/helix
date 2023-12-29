use ethereum_consensus::{capella::Withdrawal, phase0::mainnet::SLOTS_PER_EPOCH};
use reth_primitives::{proofs, Address};

pub mod request_encoding;
pub mod serde;
pub mod signing;

pub fn has_reached_fork(slot: u64, fork_epoch: u64) -> bool {
    if fork_epoch == 0 {
        return false;
    }

    let current_epoch = slot / SLOTS_PER_EPOCH;
    current_epoch >= fork_epoch
}

// TODO: really need to fix the common here. Should probably just use reth common
pub fn calculate_withdrawals_root(withdrawals: &[Withdrawal]) -> [u8; 32] {
    let reth_withdrawals: Vec<reth_primitives::Withdrawal> =
        withdrawals.iter().cloned().map(to_reth_withdrawal).collect();
    proofs::calculate_withdrawals_root(&reth_withdrawals).0
}

fn to_reth_withdrawal(withdrawal: Withdrawal) -> reth_primitives::Withdrawal {
    reth_primitives::Withdrawal {
        index: withdrawal.index as u64,
        validator_index: withdrawal.validator_index as u64,
        address: Address::from_slice(withdrawal.address.as_ref()),
        amount: withdrawal.amount,
    }
}
