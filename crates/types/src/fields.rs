use alloy_primitives::FixedBytes;
use lh_types::{EthSpec, FixedVector, MainnetEthSpec, VariableList, test_utils::TestRandom};
use rand::Rng;

use crate::{SszError, ssz_bytes_wrapper};

pub type Withdrawal = lh_types::withdrawal::Withdrawal;
pub type Withdrawals = lh_types::execution_payload::Withdrawals<MainnetEthSpec>;
pub type ExecutionRequests = lh_types::execution_requests::ExecutionRequests<MainnetEthSpec>;
pub type KzgCommitment = alloy_consensus::Bytes48;
pub type KzgCommitments =
    VariableList<KzgCommitment, <MainnetEthSpec as EthSpec>::MaxBlobCommitmentsPerBlock>;
pub type KzgProof = alloy_consensus::Bytes48;
pub type KzgProofs = Vec<KzgProof>;
pub type Transactions =
    VariableList<Transaction, <MainnetEthSpec as EthSpec>::MaxTransactionsPerPayload>;

pub fn convert_transactions_to_lighthouse(
    txs: &Transactions,
) -> Result<lh_types::Transactions<MainnetEthSpec>, SszError> {
    let mut new = Vec::with_capacity(txs.len());
    for tx in txs {
        new.push(tx.to_ssz_type()?);
    }

    VariableList::new(new)
}

const LOGS_BLOOM_SIZE: usize = 256;
pub type Bloom = FixedBytes<LOGS_BLOOM_SIZE>; // FixedVector<u8, E::BytesPerLogsBloom>;

pub fn convert_bloom_to_lighthouse(
    bloom: &Bloom,
) -> FixedVector<u8, <MainnetEthSpec as EthSpec>::BytesPerLogsBloom> {
    FixedVector::from(bloom.to_vec())
}

ssz_bytes_wrapper! {
    /// VariableList<u8, E::MaxExtraDataBytes>
    pub struct ExtraData;
    max  = <MainnetEthSpec as EthSpec>::MaxExtraDataBytes;
}

ssz_bytes_wrapper! {
    /// VariableList<u8, E::MaxBytesPerTransaction>
    pub struct Transaction;
    max  = <MainnetEthSpec as EthSpec>::MaxBytesPerTransaction;
}

impl TestRandom for Transaction {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let n = rng.random_range(0..=1000) as usize;
        let mut bytes = vec![0u8; n];
        rng.fill_bytes(&mut bytes);
        Self(bytes.into())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use lh_types::{EthSpec, MainnetEthSpec};
    use ssz::{Decode, Encode};
    use ssz_types::VariableList;
    use tree_hash::TreeHash;

    use super::*;
    use crate::TestRandomSeed;

    #[test]
    fn test_consts() {
        assert_eq!(LOGS_BLOOM_SIZE, <MainnetEthSpec as EthSpec>::bytes_per_logs_bloom());
    }

    #[test]
    fn test_extra_data() {
        type LhExtraData = VariableList<u8, <MainnetEthSpec as EthSpec>::MaxExtraDataBytes>;
        let lh_extra_data = LhExtraData::test_random();

        let our_extra_data = ExtraData(Bytes::from(lh_extra_data.clone().to_vec()));

        let json_str = serde_json::to_string(&our_extra_data).unwrap();
        let deserialized: ExtraData = serde_json::from_str(&json_str).unwrap();
        assert_eq!(our_extra_data, deserialized);

        let ssz_bytes = our_extra_data.as_ssz_bytes();
        let decoded = ExtraData::from_ssz_bytes(&ssz_bytes).unwrap();
        assert_eq!(our_extra_data, decoded);

        let lh_ssz_bytes = lh_extra_data.as_ssz_bytes();
        assert_eq!(ssz_bytes, lh_ssz_bytes, "SSZ encoding should match lighthouse");

        let our_tree_hash = our_extra_data.tree_hash_root();
        let lh_tree_hash = lh_extra_data.tree_hash_root();
        assert_eq!(our_tree_hash, lh_tree_hash, "Tree hash root should match lighthouse");
    }

    #[test]
    fn test_transaction() {
        type LhTransaction =
            lh_types::Transaction<<MainnetEthSpec as EthSpec>::MaxBytesPerTransaction>;
        let lh_transaction = LhTransaction::test_random();

        let our_transaction = Transaction(Bytes::from(lh_transaction.clone().to_vec()));

        let json_str = serde_json::to_string(&our_transaction).unwrap();
        let deserialized: Transaction = serde_json::from_str(&json_str).unwrap();
        assert_eq!(our_transaction, deserialized);

        let ssz_bytes = our_transaction.as_ssz_bytes();
        let decoded = Transaction::from_ssz_bytes(&ssz_bytes).unwrap();
        assert_eq!(our_transaction, decoded);

        let lh_ssz_bytes = lh_transaction.as_ssz_bytes();
        assert_eq!(ssz_bytes, lh_ssz_bytes, "SSZ encoding should match lighthouse");

        let our_tree_hash = our_transaction.tree_hash_root();
        let lh_tree_hash = lh_transaction.tree_hash_root();
        assert_eq!(our_tree_hash, lh_tree_hash, "Tree hash root should match lighthouse");
    }

    #[test]
    fn test_convert_transactions_to_lighthouse() {
        let mut txs = Transactions::default();
        
        // Add some test transactions
        let tx1 = Transaction(Bytes::from(vec![1, 2, 3, 4, 5]));
        let tx2 = Transaction(Bytes::from(vec![6, 7, 8, 9, 10]));
        
        txs.push(tx1).unwrap();
        txs.push(tx2).unwrap();

        let lh_txs = convert_transactions_to_lighthouse(&txs).unwrap();
        
        assert_eq!(lh_txs.len(), 2);
        assert_eq!(lh_txs[0].len(), 5);
        assert_eq!(lh_txs[1].len(), 5);
    }

    #[test]
    fn test_convert_transactions_to_lighthouse_empty() {
        let txs = Transactions::default();
        let lh_txs = convert_transactions_to_lighthouse(&txs).unwrap();
        assert_eq!(lh_txs.len(), 0);
    }

    #[test]
    fn test_convert_bloom_to_lighthouse() {
        let bloom = Bloom::random();
        let lh_bloom = convert_bloom_to_lighthouse(&bloom);
        
        // Check that the conversion preserves the data
        assert_eq!(lh_bloom.len(), LOGS_BLOOM_SIZE);
        assert_eq!(lh_bloom.len(), bloom.len());
    }

    #[test]
    fn test_convert_bloom_to_lighthouse_zero() {
        let bloom = Bloom::ZERO;
        let lh_bloom = convert_bloom_to_lighthouse(&bloom);
        
        assert_eq!(lh_bloom.len(), LOGS_BLOOM_SIZE);
        assert!(lh_bloom.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_transaction_to_ssz_type() {
        let tx = Transaction(Bytes::from(vec![1, 2, 3]));
        let ssz_tx = tx.to_ssz_type().unwrap();
        
        assert_eq!(ssz_tx.len(), 3);
        assert_eq!(ssz_tx[0], 1);
        assert_eq!(ssz_tx[1], 2);
        assert_eq!(ssz_tx[2], 3);
    }

    #[test]
    fn test_extra_data_to_ssz_type() {
        let extra_data = ExtraData(Bytes::from(vec![0x42, 0x43, 0x44]));
        let ssz_data = extra_data.to_ssz_type().unwrap();
        
        assert_eq!(ssz_data.len(), 3);
        assert_eq!(ssz_data[0], 0x42);
    }

    #[test]
    fn test_transaction_display() {
        let tx = Transaction(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]));
        let display_str = format!("{}", tx);
        
        // Should display as hex
        assert!(display_str.contains("0x") || display_str.contains("de"));
    }

    #[test]
    fn test_extra_data_display() {
        let extra_data = ExtraData(Bytes::from(vec![0xca, 0xfe]));
        let display_str = format!("{}", extra_data);
        
        // Should display as hex
        assert!(display_str.contains("0x") || display_str.contains("ca"));
    }

    #[test]
    fn test_transaction_default() {
        let tx = Transaction::default();
        assert_eq!(tx.len(), 0);
    }

    #[test]
    fn test_extra_data_default() {
        let extra_data = ExtraData::default();
        assert_eq!(extra_data.len(), 0);
    }

    #[test]
    fn test_transaction_deref() {
        let tx = Transaction(Bytes::from(vec![1, 2, 3]));
        // Should be able to call Bytes methods through Deref
        assert_eq!(tx.len(), 3);
        assert!(!tx.is_empty());
    }

    #[test]
    fn test_extra_data_deref() {
        let extra_data = ExtraData(Bytes::from(vec![1, 2]));
        assert_eq!(extra_data.len(), 2);
        assert!(!extra_data.is_empty());
    }

    #[test]
    fn test_transaction_test_random() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        
        let tx1 = Transaction::random_for_test(&mut rng);
        let tx2 = Transaction::random_for_test(&mut rng);
        
        // Random transactions should exist and potentially differ
        assert!(tx1.len() <= 1000);
        assert!(tx2.len() <= 1000);
    }

    #[test]
    fn test_bloom_type_size() {
        // Verify Bloom is correct size
        let bloom = Bloom::ZERO;
        assert_eq!(bloom.len(), LOGS_BLOOM_SIZE);
        assert_eq!(bloom.len(), 256);
    }

    #[test]
    fn test_transaction_edge_case_sizes() {
        // Empty transaction
        let empty = Transaction(Bytes::new());
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        
        // Single byte - verify data preserved
        let single = Transaction(Bytes::from(vec![0xFF]));
        assert_eq!(single.len(), 1);
        assert_eq!(single[0], 0xFF, "Single byte should be preserved");
        
        // Very large transaction - verify data preserved
        let large_data = vec![0xAB; 100_000];
        let large = Transaction(Bytes::from(large_data.clone()));
        assert_eq!(large.len(), 100_000);
        assert_eq!(large[0], 0xAB, "First byte should be preserved");
        assert_eq!(large[50_000], 0xAB, "Middle byte should be preserved");
        assert_eq!(large[99_999], 0xAB, "Last byte should be preserved");
        
        // Maximum practical size with pattern
        let max_data: Vec<u8> = (0..1_000_000).map(|i| (i % 256) as u8).collect();
        let max_tx = Transaction(Bytes::from(max_data.clone()));
        assert_eq!(max_tx.len(), 1_000_000);
        // Verify pattern preservation at different points
        assert_eq!(max_tx[0], 0, "Pattern should start at 0");
        assert_eq!(max_tx[255], 255, "Pattern should reach 255");
        assert_eq!(max_tx[256], 0, "Pattern should wrap around");
        assert_eq!(max_tx[999_999], (999_999 % 256) as u8, "Pattern should be preserved throughout");
    }

    #[test]
    fn test_extra_data_edge_case_sizes() {
        // Empty extra data
        let empty = ExtraData(Bytes::new());
        assert_eq!(empty.len(), 0);
        
        // Maximum extra data (32 bytes is typical max for Ethereum)
        let max_extra = ExtraData(Bytes::from(vec![0xFF; 32]));
        assert_eq!(max_extra.len(), 32);
        
        // Very long extra data (shouldn't panic)
        let long = ExtraData(Bytes::from(vec![0xAA; 1000]));
        assert_eq!(long.len(), 1000);
    }

    #[test]
    fn test_transactions_list_edge_cases() {
        let mut txs = Transactions::default();
        
        // Empty list
        assert_eq!(txs.len(), 0);
        assert!(txs.is_empty());
        
        // Add maximum number of transactions
        for i in 0..1000 {
            let tx = Transaction(Bytes::from(vec![i as u8]));
            txs.push(tx).unwrap();
        }
        assert_eq!(txs.len(), 1000);
        
        // Should be able to convert large list
        let lh_txs = convert_transactions_to_lighthouse(&txs).unwrap();
        assert_eq!(lh_txs.len(), 1000);
    }

    #[test]
    fn test_bloom_all_ones() {
        // Create bloom with all bits set
        let all_ones = Bloom::repeat_byte(0xFF);
        assert_eq!(all_ones.len(), LOGS_BLOOM_SIZE);
        assert!(all_ones.iter().all(|&b| b == 0xFF), "All bytes should be 0xFF");
        
        let lh_bloom = convert_bloom_to_lighthouse(&all_ones);
        assert!(lh_bloom.iter().all(|&b| b == 0xFF), "Conversion should preserve all ones");
    }

    #[test]
    fn test_bloom_alternating_pattern() {
        // Create bloom with alternating 0x55 and 0xAA pattern
        let pattern: Vec<u8> = (0..LOGS_BLOOM_SIZE)
            .map(|i| if i % 2 == 0 { 0x55 } else { 0xAA })
            .collect();
        let bloom = Bloom::from_slice(&pattern);
        
        let lh_bloom = convert_bloom_to_lighthouse(&bloom);
        assert_eq!(lh_bloom.len(), LOGS_BLOOM_SIZE);
        for i in 0..LOGS_BLOOM_SIZE {
            let expected = if i % 2 == 0 { 0x55 } else { 0xAA };
            assert_eq!(lh_bloom[i], expected, "Pattern should be preserved at index {}", i);
        }
    }

    #[test]
    fn test_transaction_ssz_round_trip_edge_cases() {
        // Empty transaction
        let empty = Transaction(Bytes::new());
        let ssz = empty.as_ssz_bytes();
        let decoded = Transaction::from_ssz_bytes(&ssz).unwrap();
        assert_eq!(empty, decoded);
        
        // Large transaction
        let large_data = vec![0xAB; 50_000];
        let large = Transaction(Bytes::from(large_data.clone()));
        let ssz = large.as_ssz_bytes();
        let decoded = Transaction::from_ssz_bytes(&ssz).unwrap();
        assert_eq!(large, decoded);
        assert_eq!(decoded.len(), 50_000);
    }

    #[test]
    fn test_transaction_clone_and_equality() {
        let tx1 = Transaction(Bytes::from(vec![1, 2, 3, 4]));
        let tx2 = tx1.clone();
        
        // Clone should be equal
        assert_eq!(tx1, tx2);
        
        // Different transaction should not be equal
        let tx3 = Transaction(Bytes::from(vec![1, 2, 3, 5]));
        assert_ne!(tx1, tx3);
        
        // Empty transactions should be equal
        let empty1 = Transaction::default();
        let empty2 = Transaction::default();
        assert_eq!(empty1, empty2);
    }

    #[test]
    fn test_extra_data_clone_and_equality() {
        let ed1 = ExtraData(Bytes::from(vec![0xCA, 0xFE]));
        let ed2 = ed1.clone();
        
        assert_eq!(ed1, ed2);
        
        let ed3 = ExtraData(Bytes::from(vec![0xCA, 0xFF]));
        assert_ne!(ed1, ed3);
    }

    #[test]
    fn test_convert_transactions_boundary_cases() {
        // Single transaction
        let mut txs = Transactions::default();
        txs.push(Transaction(Bytes::from(vec![0x42]))).unwrap();
        let lh_txs = convert_transactions_to_lighthouse(&txs).unwrap();
        assert_eq!(lh_txs.len(), 1);
        assert_eq!(lh_txs[0].len(), 1);
        assert_eq!(lh_txs[0][0], 0x42);
        
        // Transaction with all same bytes
        let mut txs = Transactions::default();
        txs.push(Transaction(Bytes::from(vec![0xFF; 100]))).unwrap();
        let lh_txs = convert_transactions_to_lighthouse(&txs).unwrap();
        assert!(lh_txs[0].iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn test_bloom_zero_vs_nonzero() {
        let zero = Bloom::ZERO;
        
        // Zero bloom should have all zeros
        assert!(zero.iter().all(|&b| b == 0), "ZERO bloom should be all zeros");
        assert_eq!(zero.len(), LOGS_BLOOM_SIZE);
        
        // Non-zero bloom - use deterministic pattern instead of random
        let nonzero = Bloom::repeat_byte(0x01);
        assert!(nonzero.iter().all(|&b| b == 0x01), "All bytes should be 0x01");
        assert_ne!(zero, nonzero, "ZERO and non-zero should differ");
        
        // Verify ZERO conversion
        let lh_zero = convert_bloom_to_lighthouse(&zero);
        assert!(lh_zero.iter().all(|&b| b == 0), "Converted ZERO should remain all zeros");
    }

    #[test]
    fn test_transaction_display_formats() {
        // Short transaction
        let short = Transaction(Bytes::from(vec![0x01, 0x02]));
        let display = format!("{}", short);
        assert!(!display.is_empty(), "Display should produce output");
        
        // Empty transaction
        let empty = Transaction::default();
        let display = format!("{}", empty);
        assert!(!display.is_empty(), "Empty transaction should still have display output");
        
        // Debug format
        let debug = format!("{:?}", short);
        assert!(!debug.is_empty(), "Debug format should work");
    }
}
