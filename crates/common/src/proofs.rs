use ethereum_consensus::{
    bellatrix::presets::minimal::Transaction,
    deneb::minimal::MAX_TRANSACTIONS_PER_PAYLOAD,
    phase0::Bytes32,
    primitives::{BlsPublicKey, BlsSignature},
    ssz::prelude::*,
};
use reth_primitives::{PooledTransactionsElement, TxHash, B256};
use sha2::{Digest, Sha256};
use tree_hash::Hash256;

// Import the new version of the `ssz-rs` crate for multiproof verification.
use ::ssz_rs as ssz;

use crate::api::constraints_api::{SignableBLS, MAX_CONSTRAINTS_PER_SLOT};

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("Leaves and indices length mismatch")]
    LengthMismatch,
    #[error("Mismatch in provided leaves and leaves to prove")]
    LeavesMismatch,
    #[error("Hash not found in constraints cache: {0:?}")]
    MissingHash(TxHash),
    #[error("Proof verification failed")]
    VerificationFailed,
    #[error("Decoding failed: {0}")]
    DecodingFailed(String),
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct InclusionProofs {
    pub transaction_hashes: List<Bytes32, MAX_CONSTRAINTS_PER_SLOT>,
    pub generalized_indexes: List<u64, MAX_CONSTRAINTS_PER_SLOT>,
    pub merkle_hashes: List<Bytes32, MAX_TRANSACTIONS_PER_PAYLOAD>,
}

impl InclusionProofs {
    /// Returns the total number of leaves in the tree.
    pub fn total_leaves(&self) -> usize {
        self.transaction_hashes.len()
    }
}

pub type HashTreeRoot = tree_hash::Hash256;

#[derive(Debug, Clone, Serializable, serde::Deserialize, serde::Serialize)]
pub struct SignedConstraints {
    pub message: ConstraintsMessage,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Serializable, Merkleized)]
pub struct ConstraintsMessage {
    pub pubkey: BlsPublicKey,
    pub slot: u64,
    pub top: bool,
    pub transactions: List<Transaction, MAX_CONSTRAINTS_PER_SLOT>,
}

impl SignableBLS for ConstraintsMessage {
    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&self.pubkey.to_vec());
        hasher.update(self.slot.to_le_bytes());
        hasher.update((self.top as u8).to_le_bytes());
        for tx in self.transactions.iter() {
            // Convert the opaque bytes to a EIP-2718 envelope and obtain the tx hash.
            // this is needed to handle type 3 transactions.
            // FIXME: don't unwrap here and handle the error properly
            let tx = PooledTransactionsElement::decode_enveloped(tx.to_vec().into()).unwrap();
            hasher.update(tx.hash().as_slice());
        }

        hasher.finalize().into()
    }
}

/// List of transaction hashes and the corresponding hash tree roots of the raw transactions.
pub type ConstraintsProofData = Vec<(TxHash, HashTreeRoot)>;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SignedConstraintsWithProofData {
    pub signed_constraints: SignedConstraints,
    pub proof_data: ConstraintsProofData,
}

impl TryFrom<SignedConstraints> for SignedConstraintsWithProofData {
    type Error = ProofError;

    fn try_from(value: SignedConstraints) -> Result<Self, ProofError> {
        let mut transactions = Vec::with_capacity(value.message.transactions.len());
        for transaction in value.message.transactions.to_vec().iter() {
            let tx = PooledTransactionsElement::decode_enveloped(transaction.to_vec().into())
                .map_err(|e| ProofError::DecodingFailed(e.to_string()))?;

            let tx_hash = *tx.hash();

            // Compute the hash tree root on the transaction object decoded without the optional
            // sidecar. this is to prevent hashing the blobs of type 3 transactions.
            let root = tx.into_transaction().envelope_encoded();
            let root = Transaction::try_from(root.as_ref())
                .map_err(|e| ProofError::DecodingFailed(e.to_string()))?;
            let root = root
                .clone()
                .hash_tree_root()
                .map_err(|e| ProofError::DecodingFailed(e.to_string()))?;
            let root = Hash256::from_slice(&root);

            transactions.push((tx_hash, root));
        }

        Ok(Self { signed_constraints: value, proof_data: transactions })
    }
}

/// Returns the length of the leaves that need to be proven (i.e.  transactions).
fn total_leaves(constraints: &[&ConstraintsProofData]) -> usize {
    constraints.iter().map(|c| c.len()).sum()
}

/// Verifies the provided multiproofs against the constraints & transactions root.
///
/// NOTE: the constraints hashes and hash tree roots must be in the same order of the transaction
/// hashes in the inclusion proofs.
pub fn verify_multiproofs(
    constraints_proofs_data: &[&ConstraintsProofData],
    proofs: &InclusionProofs,
    root: B256,
) -> Result<(), ProofError> {
    // Check if the length of the leaves and indices match
    if proofs.transaction_hashes.len() != proofs.generalized_indexes.len() {
        return Err(ProofError::LengthMismatch)
    }

    let total_leaves = total_leaves(constraints_proofs_data);

    // Check if the total leaves matches the proofs provided
    if total_leaves != proofs.total_leaves() {
        return Err(ProofError::LeavesMismatch)
    }

    // Get all the leaves from the saved constraints
    let mut leaves = Vec::with_capacity(proofs.total_leaves());

    // NOTE: Get the leaves from the constraints cache by matching the saved hashes.
    // We need the leaves in order to verify the multiproof.
    for hash in proofs.transaction_hashes.iter() {
        let mut found = false;
        for constraints_proof in constraints_proofs_data {
            for (saved_hash, leaf) in *constraints_proof {
                if saved_hash.as_slice() == hash.as_slice() {
                    found = true;
                    leaves.push(B256::from(leaf.0));
                    break
                }
            }
            if found {
                break
            }
        }

        // If the hash is not found in the constraints cache, return an error
        if !found {
            return Err(ProofError::MissingHash(TxHash::from_slice(hash.as_slice())))
        }
    }

    // Conversions to the correct types (and versions of the same type)
    let leaves = leaves.into_iter().map(|h| h.as_slice().try_into().unwrap()).collect::<Vec<_>>();
    let merkle_proofs = proofs
        .merkle_hashes
        .to_vec()
        .iter()
        .map(|h| h.as_slice().try_into().unwrap())
        .collect::<Vec<_>>();
    let indexes =
        proofs.generalized_indexes.to_vec().iter().map(|h| *h as usize).collect::<Vec<_>>();
    let root = root.as_slice().try_into().expect("Invalid root length");

    // Verify the Merkle multiproof against the root
    ssz::multiproofs::verify_merkle_multiproof(
        leaves.as_slice(),
        merkle_proofs.as_ref(),
        indexes.as_slice(),
        root,
    )
    .map_err(|_| ProofError::VerificationFailed)?;

    Ok(())
}
