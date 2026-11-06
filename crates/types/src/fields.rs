use std::marker::PhantomData;

use alloy_primitives::FixedBytes;
use lh_types::{EthSpec, MainnetEthSpec};
use rand::Rng;
use ssz_types::{FixedVector, ProgressiveVariableList, VariableList};

use crate::{ExecutionRequestsGloas, SszError, TestRandom, ssz_bytes_wrapper};

pub type Withdrawal = lh_types::Withdrawal;
pub type Withdrawals = lh_types::Withdrawals<MainnetEthSpec>;
// `ExecutionRequests` is a fork-versioned superstruct as of Gloas (which adds
// `builder_deposits`/`builder_exits`, EIP-8282). The three pre-Gloas lists keep the Electra
// shape; a Gloas submission carries the two builder lists beside it, the way it carries the
// block access list, so the pre-Gloas bytes never move.
pub type ExecutionRequests = lh_types::ExecutionRequestsElectra<MainnetEthSpec>;
pub type BuilderDepositRequests = ProgressiveVariableList<lh_types::BuilderDepositRequest>;
pub type BuilderExitRequests = ProgressiveVariableList<lh_types::BuilderExitRequest>;
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

/// Real, progressive-list Gloas transactions shape, per EIP-7688.
pub fn convert_transactions_to_progressive(
    txs: &Transactions,
) -> lh_types::ProgressiveTransactions {
    ProgressiveVariableList::new(
        txs.iter().map(|tx| ProgressiveVariableList::new(tx.as_ref().to_vec())).collect(),
    )
}

/// Every execution request a node produced, split by EIP-7685 type prefix. The three
/// pre-Gloas lists keep the shape a submission has always carried; EIP-8282's builder
/// deposits and exits ride beside them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DecodedRequests {
    pub requests: ExecutionRequests,
    pub builder_deposits: BuilderDepositRequests,
    pub builder_exits: BuilderExitRequests,
}

impl DecodedRequests {
    /// Each entry is a one-byte type prefix followed by that type's SSZ list. This does not
    /// go through alloy's `ExecutionRequestsV4`, which knows only prefixes 0 to 2.
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, String> {
        fn ssz_decode<T: ssz::Decode>(body: &[u8], what: &str) -> Result<T, String> {
            T::from_ssz_bytes(body).map_err(|e| format!("{what}: {e:?}"))
        }
        let mut decoded = DecodedRequests::default();
        for entry in entries {
            let (prefix, body) = entry.split_first().ok_or("empty execution request")?;
            let request_type = crate::RequestType::from_u8(*prefix)
                .ok_or_else(|| format!("unknown request_type prefix: {prefix}"))?;
            match request_type {
                crate::RequestType::Deposit => {
                    decoded.requests.deposits = ssz_decode(body, "deposits")?;
                }
                crate::RequestType::Withdrawal => {
                    decoded.requests.withdrawals = ssz_decode(body, "withdrawals")?;
                }
                crate::RequestType::Consolidation => {
                    decoded.requests.consolidations = ssz_decode(body, "consolidations")?;
                }
                crate::RequestType::BuilderDeposit => {
                    decoded.builder_deposits = ssz_decode(body, "builder deposits")?;
                }
                crate::RequestType::BuilderExit => {
                    decoded.builder_exits = ssz_decode(body, "builder exits")?;
                }
            }
        }
        Ok(decoded)
    }

    pub fn to_gloas(&self) -> ExecutionRequestsGloas {
        execution_requests_to_gloas(&self.requests, &self.builder_deposits, &self.builder_exits)
    }
}

/// Converts helix's Electra-shaped builder-submission execution requests, plus the EIP-8282
/// builder lists a Gloas submission carries beside them, into the real Gloas shape.
pub fn execution_requests_to_gloas(
    requests: &ExecutionRequests,
    builder_deposits: &BuilderDepositRequests,
    builder_exits: &BuilderExitRequests,
) -> ExecutionRequestsGloas {
    ExecutionRequestsGloas {
        deposits: requests.deposits.iter().cloned().collect(),
        withdrawals: requests.withdrawals.iter().cloned().collect(),
        consolidations: requests.consolidations.iter().cloned().collect(),
        builder_deposits: builder_deposits.iter().cloned().collect(),
        builder_exits: builder_exits.iter().cloned().collect(),
        _phantom: PhantomData,
    }
}

const LOGS_BLOOM_SIZE: usize = 256;
pub type Bloom = FixedBytes<LOGS_BLOOM_SIZE>; // FixedVector<u8, E::BytesPerLogsBloom>;

pub fn convert_bloom_to_lighthouse(
    bloom: &Bloom,
) -> FixedVector<u8, <MainnetEthSpec as EthSpec>::BytesPerLogsBloom> {
    FixedVector::new(bloom.to_vec()).expect("Bloom is always BytesPerLogsBloom bytes")
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

ssz_bytes_wrapper! {
    /// The opaque encoded EIP-7928 block access list, as the builder produced
    /// it. Gloas's own `BlockAccessList` is a `ProgressiveVariableList<u8>`,
    /// so nothing here mirrors its structure.
    pub struct BlockAccessListBytes;
    max  = <MainnetEthSpec as EthSpec>::MaxBytesPerTransaction;
}

impl TestRandom for BlockAccessListBytes {
    fn random_for_test(rng: &mut impl rand::RngCore) -> Self {
        let n = rng.random_range(0..=1000) as usize;
        let mut bytes = vec![0u8; n];
        rng.fill_bytes(&mut bytes);
        Self(bytes.into())
    }
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
    fn convert_transactions_to_progressive_preserves_bytes() {
        let txs = Transactions::random_for_test(&mut rand::rng());

        let progressive = convert_transactions_to_progressive(&txs);

        assert_eq!(progressive.len(), txs.len());
        for (converted, original) in progressive.as_slice().iter().zip(txs.iter()) {
            assert_eq!(converted.as_slice(), original.as_ref());
        }
    }

    /// EIP-8282's builder deposits and exits reach the consensus shape the proposer signs.
    /// While they were dropped here, a block carrying one could not be bid at all.
    #[test]
    fn execution_requests_to_gloas_carries_every_list() {
        let requests = ExecutionRequests::random_for_test(&mut rand::rng());
        let builder_deposits: BuilderDepositRequests = vec![lh_types::BuilderDepositRequest {
            pubkey: lh_bls::PublicKeyBytes::empty(),
            withdrawal_credentials: alloy_primitives::B256::repeat_byte(0x11),
            amount: 32_000_000_000,
            signature: lh_bls::SignatureBytes::empty(),
        }]
        .into();
        let builder_exits: BuilderExitRequests = vec![lh_types::BuilderExitRequest {
            source_address: alloy_primitives::Address::repeat_byte(0x22),
            pubkey: lh_bls::PublicKeyBytes::empty(),
        }]
        .into();

        let gloas = execution_requests_to_gloas(&requests, &builder_deposits, &builder_exits);

        assert!(gloas.deposits.iter().eq(requests.deposits.iter()));
        assert!(gloas.withdrawals.iter().eq(requests.withdrawals.iter()));
        assert!(gloas.consolidations.iter().eq(requests.consolidations.iter()));
        assert!(gloas.builder_deposits.iter().eq(builder_deposits.iter()));
        assert!(gloas.builder_exits.iter().eq(builder_exits.iter()));
    }
}
