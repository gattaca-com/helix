use std::collections::HashMap;

use alloy_consensus::TxEnvelope;
use alloy_rlp::Decodable;
use helix_types::{BlsPublicKeyBytes, Transaction};

use crate::messages::InclusionList;

pub(crate) fn compute_shared_inclusion_list(
    vote_map: &mut HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
    slot: u64,
    inclusion_list: InclusionList,
) -> InclusionList {
    let mut tx_frequency = HashMap::new();
    vote_map.retain(|_, (il_slot, _)| *il_slot >= slot);
    for il in vote_map.iter().map(|(_, (_, il))| il).chain(std::iter::once(&inclusion_list)) {
        for tx in il.iter() {
            let Ok(decoded_tx) = TxEnvelope::decode(&mut &tx[..]) else {
                continue;
            };

            tx_frequency
                .entry(*decoded_tx.tx_hash())
                .and_modify(|(_, c)| *c += 1)
                .or_insert((tx.clone(), 1));
        }
    }
    let mut tx_frequency_ordered: Vec<_> = tx_frequency.into_iter().collect();
    // Order descending by (frequency, tx_hash)
    tx_frequency_ordered.sort_unstable_by(|(tx_hash1, (_, freq1)), (tx_hash2, (_, freq2))| {
        freq2.cmp(freq1).then(tx_hash2.cmp(tx_hash1))
    });

    let mut bytes_available = 8000;
    let final_il: Vec<_> = tx_frequency_ordered
        .into_iter()
        .map(|(_, (tx, _))| tx.into())
        .filter(|tx: &Transaction| {
            if tx.len() > bytes_available {
                false
            } else {
                bytes_available -= tx.len();
                true
            }
        })
        .collect();

    final_il.into()
}

#[cfg(test)]
mod tests {
    // TODO: add unit tests
}
