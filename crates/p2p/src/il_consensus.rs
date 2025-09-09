use std::collections::HashMap;

use alloy_consensus::TxEnvelope;
use alloy_rlp::Decodable;
use helix_types::{BlsPublicKeyBytes, Transaction};
use tree_hash::TreeHash;

use crate::messages::InclusionList;

pub(crate) fn compute_shared_inclusion_list(
    vote_map: &HashMap<BlsPublicKeyBytes, (u64, InclusionList)>,
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
    use alloy_consensus::TxEip1559;
    use alloy_primitives::{Bytes, Signature};
    use alloy_rlp::Encodable;

    use super::*;

    fn create_tx(nonce: u64) -> Bytes {
        let dummy_tx = TxEip1559 {
            chain_id: 42,
            nonce,
            gas_limit: 42,
            max_fee_per_gas: 42,
            max_priority_fee_per_gas: 42,
            to: Default::default(),
            value: Default::default(),
            access_list: Default::default(),
            input: Default::default(),
        };
        let mut buf = vec![];
        TxEnvelope::new_unhashed(
            dummy_tx.into(),
            Signature::new(Default::default(), Default::default(), Default::default()),
        )
        .encode(&mut buf);
        buf.into()
    }

    fn create_full_il(start: u64) -> Vec<Transaction> {
        let mut remaining_bytes = 8000;
        (start..)
            .map(create_tx)
            .take_while(|tx| {
                if remaining_bytes < tx.len() {
                    false
                } else {
                    remaining_bytes -= tx.len();
                    true
                }
            })
            .map(Transaction)
            .collect()
    }

    #[test]
    fn test_compute_shared_inclusion_list_simple() {
        // 3 ILs, all the same
        let txs: Vec<Transaction> = create_full_il(0);

        let vote_map = HashMap::from([
            ([0_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
            ([1_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
            ([2_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
        ]);
        let slot = 1;
        let inclusion_list = InclusionList::default();
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list);

        assert_eq!(shared_il.len(), txs.len());

        for tx in shared_il {
            assert!(txs.contains(&tx));
        }
    }

    #[test]
    fn test_compute_shared_inclusion_list_one_different() {
        // 3 ILs, two same, one different
        let txs: Vec<Transaction> = create_full_il(0);
        let txs_different: Vec<Transaction> = create_full_il(1000);

        let vote_map = HashMap::from([
            ([0_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
            ([1_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
            ([2_u8; 48].into(), (1, InclusionList::new(txs_different).unwrap())),
        ]);
        let slot = 1;
        let inclusion_list = InclusionList::default();
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list);

        assert_eq!(shared_il.len(), txs.len());

        for tx in shared_il {
            assert!(txs.contains(&tx));
        }
    }

    #[test]
    fn test_compute_shared_inclusion_list_shuffled() {
        // Three ILs, with some transactions that appear in multiple ILs
        let txs: Vec<Transaction> = create_full_il(0);
        // 200 random txs, and the first 200 txs from the main IL
        let txs_1: Vec<Transaction> = create_full_il(1000)
            .into_iter()
            .take(200)
            .chain(txs.iter().cloned().take(250))
            .collect();
        // 200 random txs, and the rest of txs from the main IL
        let txs_2: Vec<Transaction> = create_full_il(2000)
            .into_iter()
            .take(200)
            .chain(txs.iter().cloned().skip(250))
            .collect();

        let vote_map = HashMap::from([
            ([0_u8; 48].into(), (1, InclusionList::new(txs.clone()).unwrap())),
            ([1_u8; 48].into(), (1, InclusionList::new(txs_1).unwrap())),
            ([2_u8; 48].into(), (1, InclusionList::new(txs_2).unwrap())),
        ]);
        let slot = 1;
        let inclusion_list = InclusionList::default();
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list);

        assert_eq!(shared_il.len(), txs.len());

        for tx in shared_il {
            assert!(txs.contains(&tx));
        }
    }
}
