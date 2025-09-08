use std::collections::HashMap;

use alloy_consensus::TxEnvelope;
use alloy_rlp::Decodable;
use helix_types::{BlsPublicKeyBytes, Transaction};
use tree_hash::TreeHash;

use crate::messages::InclusionList;

pub(crate) fn compute_shared_inclusion_list(
    vote_map: &mut HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
    slot: u64,
    inclusion_list: InclusionList,
) -> InclusionList {
    let mut tx_frequency = HashMap::new();
    for (il_slot, il) in vote_map
        .iter()
        .map(|(_, (slot, il))| (slot, il))
        .chain(std::iter::once((&slot, &inclusion_list)))
    {
        if *il_slot != slot {
            continue;
        }
        for tx in il.iter() {
            let Ok(decoded_tx) = TxEnvelope::decode(&mut &tx[..]) else {
                continue;
            };

            tx_frequency
                .entry(*decoded_tx.tx_hash())
                .and_modify(|(_, f)| *f += 1)
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

pub(crate) fn compute_final_inclusion_list(
    vote_map: HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
    slot: u64,
    inclusion_list: InclusionList,
) -> InclusionList {
    let mut il_by_frequency = HashMap::new();
    il_by_frequency.insert(inclusion_list.tree_hash_root(), (inclusion_list, 1));
    vote_map.into_iter().for_each(|(_, (il_slot, il))| {
        if il_slot != slot {
            return;
        }
        let il_hash = il.tree_hash_root();
        il_by_frequency.entry(il_hash).and_modify(|(_, f)| *f += 1).or_insert((il, 1));
    });
    // Get the max, ordered by frequency, total size, and hash
    let (_, (final_il, _)) = il_by_frequency
        .into_iter()
        .max_by_key(|(il_hash, (il, freq))| {
            (*freq, il.iter().map(|tx| tx.len()).sum::<usize>(), *il_hash)
        })
        .expect("map has at least one entry");

    final_il.into_iter().collect::<Vec<_>>().into()
}

#[cfg(test)]
mod tests {
    // TODO: add unit tests
}
