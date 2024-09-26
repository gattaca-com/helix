use ::serde::de;
use ethereum_consensus::{
    altair::{Bytes32, Slot},
    capella::Withdrawal,
    phase0::mainnet::SLOTS_PER_EPOCH,
    ssz::{self, prelude::SimpleSerialize},
};
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
    let reth_withdrawals: Vec<reth_primitives::Withdrawal> = withdrawals.iter().cloned().map(to_reth_withdrawal).collect();
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

pub fn try_decode_into<T>(is_ssz: bool, body_bytes: &[u8], json_fallback: bool) -> Option<T>
where
    T: SimpleSerialize + de::DeserializeOwned,
{
    if is_ssz {
        match ssz::prelude::deserialize::<T>(body_bytes).ok() {
            Some(decoded) => Some(decoded),
            None => {
                if json_fallback {
                    serde_json::from_slice(body_bytes).ok()
                } else {
                    None
                }
            }
        }
    } else {
        serde_json::from_slice(body_bytes).ok()
    }
}

pub fn get_payload_attributes_key(parent_hash: &Bytes32, slot: Slot) -> String {
    format!("{parent_hash:?}:{slot}")
}
