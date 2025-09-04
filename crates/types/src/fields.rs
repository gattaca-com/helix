use alloy_eips::eip7691::MAX_BLOBS_PER_BLOCK_ELECTRA;
use alloy_primitives::{Bytes, FixedBytes};
use lh_types::{test_utils::TestRandom, EthSpec, FixedVector, MainnetEthSpec, VariableList};
use rand::Rng;
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};

use crate::{ssz_list_wrapper, SszError};

pub type Withdrawal = lh_types::withdrawal::Withdrawal;
pub type Withdrawals = lh_types::execution_payload::Withdrawals<MainnetEthSpec>;
pub type ExecutionRequests = lh_types::execution_requests::ExecutionRequests<MainnetEthSpec>;
pub type KzgCommitment = alloy_consensus::Bytes48;
pub type KzgProof = alloy_consensus::Bytes48;
pub type KzgProofs = Vec<KzgProof>;

const LOGS_BLOOM_SIZE: usize = 256;
pub type Bloom = FixedBytes<LOGS_BLOOM_SIZE>; // FixedVector<u8, E::BytesPerLogsBloom>;

pub fn convert_bloom_to_lighthouse(
    bloom: &Bloom,
) -> FixedVector<u8, <MainnetEthSpec as EthSpec>::BytesPerLogsBloom> {
    FixedVector::from(bloom.to_vec())
}

ssz_list_wrapper! {
    /// VariableList<u8, E::MaxExtraDataBytes>
    pub struct ExtraData(Bytes);
    elem = u8;
    max  = <MainnetEthSpec as EthSpec>::MaxExtraDataBytes;
}

impl ExtraData {
    pub fn to_ssz_type(
        &self,
    ) -> Result<VariableList<u8, <MainnetEthSpec as EthSpec>::MaxExtraDataBytes>, SszError> {
        VariableList::new(self.0.as_ref().to_vec())
    }
}

impl std::fmt::Display for ExtraData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

ssz_list_wrapper! {
    /// VariableList<KzgCommitment, E::MaxBlobCommitmentsPerBlock>
    pub struct KzgCommitments(::std::vec::Vec<KzgCommitment>);
    elem = KzgCommitment;
    max  = <MainnetEthSpec as EthSpec>::MaxBlobCommitmentsPerBlock;
}

impl TestRandom for KzgCommitments {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let n = rng.gen_range(0..=MAX_BLOBS_PER_BLOCK_ELECTRA) as usize;
        let mut commitments = Vec::with_capacity(n);
        for _ in 0..n {
            commitments.push(KzgCommitment::random());
        }
        Self(commitments)
    }
}

impl KzgCommitments {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
}

ssz_list_wrapper! {
    /// VariableList<u8, E::MaxBytesPerTransaction>
    pub struct Transaction(Bytes);
    elem = u8;
    max  = <MainnetEthSpec as EthSpec>::MaxBytesPerTransaction;
}

ssz_list_wrapper! {
    /// VariableList<Transaction, E::MaxTransactionsPerPayload>
    pub struct Transactions(::std::vec::Vec<Transaction>);
    elem = Transaction;
    max  = <MainnetEthSpec as EthSpec>::MaxTransactionsPerPayload;
}

impl Transactions {
    pub fn to_ssz_type(&self) -> Result<lh_types::Transactions<MainnetEthSpec>, SszError> {
        let mut transactions = Vec::with_capacity(self.0.len());

        for tx in self.0.iter() {
            let tx: VariableList<u8, <MainnetEthSpec as EthSpec>::MaxBytesPerTransaction> =
                VariableList::new(tx.as_ref().to_vec())?;
            transactions.push(tx);
        }

        VariableList::new(transactions)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;
    use lh_types::{EthSpec, MainnetEthSpec};
    use serde_json;
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
    fn test_kzg_commitments() {
        type LhKzgCommitments = VariableList<
            lh_types::KzgCommitment,
            <MainnetEthSpec as EthSpec>::MaxBlobCommitmentsPerBlock,
        >;
        let lh_kzg_commitments = LhKzgCommitments::test_random();

        let our_commitments: Vec<KzgCommitment> =
            lh_kzg_commitments.iter().map(|commitment| KzgCommitment::from(commitment.0)).collect();
        let our_kzg_commitments = KzgCommitments(our_commitments);

        let json_str = serde_json::to_string(&our_kzg_commitments).unwrap();
        let deserialized: KzgCommitments = serde_json::from_str(&json_str).unwrap();
        assert_eq!(our_kzg_commitments, deserialized);

        let ssz_bytes = our_kzg_commitments.as_ssz_bytes();
        let decoded = KzgCommitments::from_ssz_bytes(&ssz_bytes).unwrap();
        assert_eq!(our_kzg_commitments, decoded);

        let lh_ssz_bytes = lh_kzg_commitments.as_ssz_bytes();
        assert_eq!(ssz_bytes, lh_ssz_bytes, "SSZ encoding should match lighthouse");

        let our_tree_hash = our_kzg_commitments.tree_hash_root();
        let lh_tree_hash = lh_kzg_commitments.tree_hash_root();
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
    fn test_transactions() {
        type LhTransactions = lh_types::Transactions<MainnetEthSpec>;
        let lh_transactions = LhTransactions::test_random();

        let our_transactions_vec: Vec<Transaction> =
            lh_transactions.iter().map(|tx| Transaction(Bytes::from(tx.to_vec()))).collect();
        let our_transactions = Transactions(our_transactions_vec);

        let json_str = serde_json::to_string(&our_transactions).unwrap();
        let deserialized: Transactions = serde_json::from_str(&json_str).unwrap();
        assert_eq!(our_transactions, deserialized);

        let ssz_bytes = our_transactions.as_ssz_bytes();
        let decoded = Transactions::from_ssz_bytes(&ssz_bytes).unwrap();
        assert_eq!(our_transactions, decoded);

        let lh_ssz_bytes = lh_transactions.as_ssz_bytes();
        assert_eq!(ssz_bytes, lh_ssz_bytes, "SSZ encoding should match lighthouse");

        let our_tree_hash = our_transactions.tree_hash_root();
        let lh_tree_hash = lh_transactions.tree_hash_root();
        assert_eq!(our_tree_hash, lh_tree_hash, "Tree hash root should match lighthouse");
    }
}
