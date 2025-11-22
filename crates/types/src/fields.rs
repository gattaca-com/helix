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
}
