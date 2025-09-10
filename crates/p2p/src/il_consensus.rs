use std::collections::HashMap;

use alloy_consensus::TxEnvelope;
use alloy_rlp::Decodable;
use helix_common::api::builder_api::InclusionList;
use helix_types::BlsPublicKeyBytes;
use tree_hash::TreeHash;

const INCLUSION_LIST_MAX_BYTES: usize = 8000;

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
        for tx in il.txs.iter() {
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

    let mut bytes_available = INCLUSION_LIST_MAX_BYTES;
    let txs: Vec<_> = tx_frequency_ordered
        .into_iter()
        .map(|(_, (tx, _))| tx)
        .filter(|tx| {
            if tx.len() > bytes_available {
                false
            } else {
                bytes_available -= tx.len();
                true
            }
        })
        .collect();

    // Truncate if over
    InclusionList { txs: txs.into() }
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
            (*freq, il.txs.iter().map(|tx| tx.len()).sum::<usize>(), *il_hash)
        })
        .expect("map has at least one entry");

    final_il
}

#[cfg(test)]
mod tests {
    use alloy_consensus::TxEip1559;
    use alloy_primitives::Signature;
    use alloy_rlp::Encodable;
    use helix_types::Transaction;

    use super::*;

    fn create_tx(nonce: u64) -> Transaction {
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
        Transaction(buf.into())
    }

    fn create_full_il(start: u64) -> InclusionList {
        let mut remaining_bytes = INCLUSION_LIST_MAX_BYTES;
        let txs: Vec<_> = (start..)
            .map(create_tx)
            .take_while(|tx| {
                if remaining_bytes < tx.len() {
                    false
                } else {
                    remaining_bytes -= tx.len();
                    true
                }
            })
            .collect();
        InclusionList { txs: txs.into() }
    }

    #[test]
    fn test_compute_shared_inclusion_list_simple() {
        // 3 ILs, all the same
        let slot = 1;
        let inclusion_list = create_full_il(0);

        let vote_map = HashMap::from([
            ([1_u8; 48].into(), (slot, inclusion_list.clone())),
            ([2_u8; 48].into(), (slot, inclusion_list.clone())),
        ]);
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list.clone());

        assert_eq!(shared_il.txs.len(), inclusion_list.txs.len());

        for tx in shared_il.txs {
            assert!(inclusion_list.txs.contains(&tx));
        }
    }

    #[test]
    fn test_compute_shared_inclusion_list_one_different() {
        // 3 ILs, two same, one different
        let slot = 1;
        let inclusion_list = create_full_il(0);

        let il_different = create_full_il(1000);

        let vote_map = HashMap::from([
            ([1_u8; 48].into(), (slot, inclusion_list.clone())),
            ([2_u8; 48].into(), (slot, il_different)),
        ]);
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list.clone());

        assert_eq!(shared_il.txs.len(), inclusion_list.txs.len());

        for tx in shared_il.txs {
            assert!(inclusion_list.txs.contains(&tx));
        }
    }

    #[test]
    fn test_compute_shared_inclusion_list_shuffled() {
        // 3 ILs, with some transactions that appear in multiple ILs
        let slot = 1;
        let inclusion_list = create_full_il(0);
        // 200 random txs, and the first 200 txs from the main IL
        let txs_1: Vec<_> = create_full_il(1000)
            .txs
            .into_iter()
            .take(200)
            .chain(inclusion_list.txs.iter().cloned().take(250))
            .collect();
        // 200 random txs, and the rest of txs from the main IL
        let txs_2: Vec<_> = create_full_il(2000)
            .txs
            .into_iter()
            .take(200)
            .chain(inclusion_list.txs.iter().cloned().skip(250))
            .collect();

        let vote_map = HashMap::from([
            ([1_u8; 48].into(), (slot, InclusionList { txs: txs_1.into() })),
            ([2_u8; 48].into(), (slot, InclusionList { txs: txs_2.into() })),
        ]);
        let shared_il = compute_shared_inclusion_list(&vote_map, slot, inclusion_list.clone());

        assert_eq!(shared_il.txs.len(), inclusion_list.txs.len());

        for tx in shared_il.txs {
            assert!(inclusion_list.txs.contains(&tx));
        }
    }

    #[test]
    fn test_compute_final_inclusion_list_simple() {
        // 3 ILs, all the same
        let slot = 1;
        let inclusion_list = create_full_il(0);

        let vote_map = HashMap::from([
            ([1_u8; 48].into(), (slot, inclusion_list.clone())),
            ([2_u8; 48].into(), (slot, inclusion_list.clone())),
        ]);
        let final_il = compute_final_inclusion_list(vote_map, slot, inclusion_list.clone());

        assert_eq!(final_il, inclusion_list);
    }

    #[test]
    fn test_compute_final_inclusion_list_one_different() {
        // 3 ILs, two same, one different
        let slot = 1;
        let inclusion_list = create_full_il(0);
        let il_different = create_full_il(1000);

        let vote_map = HashMap::from([
            ([1_u8; 48].into(), (slot, inclusion_list.clone())),
            ([2_u8; 48].into(), (slot, il_different)),
        ]);
        let final_il = compute_final_inclusion_list(vote_map, slot, inclusion_list.clone());

        assert_eq!(final_il, inclusion_list);
    }
}
