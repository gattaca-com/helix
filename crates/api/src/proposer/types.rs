use alloy_primitives::B256;
use helix_common::Filtering;
use helix_types::{
    BeaconBlockBodyElectra, BeaconBlockElectra, BlsPublicKeyBytes, PayloadAndBlobs,
    SignedBeaconBlock, SignedBeaconBlockElectra, SignedBlindedBeaconBlock, VersionedSignedProposal,
};
use serde::Deserialize;

use crate::proposer::error::ProposerApiError;

pub const GET_HEADER_REQUEST_CUTOFF_MS: i64 = 3000;

#[derive(Debug, Deserialize)]
pub struct GetHeaderParams {
    pub slot: u64,
    pub parent_hash: B256,
    pub pubkey: BlsPublicKeyBytes,
}

pub fn unblind_beacon_block(
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    versioned_execution_payload: &PayloadAndBlobs,
) -> Result<VersionedSignedProposal, ProposerApiError> {
    match signed_blinded_beacon_block {
        SignedBlindedBeaconBlock::Altair(_) |
        SignedBlindedBeaconBlock::Base(_) |
        SignedBlindedBeaconBlock::Bellatrix(_) |
        SignedBlindedBeaconBlock::Capella(_) |
        SignedBlindedBeaconBlock::Deneb(_) |
        SignedBlindedBeaconBlock::Fulu(_) => Err(ProposerApiError::UnsupportedBeaconChainVersion),
        SignedBlindedBeaconBlock::Electra(blinded_block) => {
            let signature = blinded_block.signature.clone();
            let block = &blinded_block.message;
            let body = &block.body;
            let execution_payload = versioned_execution_payload
                .execution_payload
                .clone()
                .to_lighthouse_electra_paylaod()
                .map_err(ProposerApiError::SszError)?;

            let blobs_bundle = &versioned_execution_payload.blobs_bundle;

            if body.blob_kzg_commitments.len() != blobs_bundle.blobs.len() {
                return Err(ProposerApiError::BlindedBlobsBundleLengthMismatch);
            }

            let inner = SignedBeaconBlockElectra {
                message: BeaconBlockElectra {
                    slot: block.slot,
                    proposer_index: block.proposer_index,
                    parent_root: block.parent_root,
                    state_root: block.state_root,
                    body: BeaconBlockBodyElectra {
                        randao_reveal: body.randao_reveal.clone(),
                        eth1_data: body.eth1_data.clone(),
                        graffiti: body.graffiti,
                        proposer_slashings: body.proposer_slashings.clone(),
                        attester_slashings: body.attester_slashings.clone(),
                        attestations: body.attestations.clone(),
                        deposits: body.deposits.clone(),
                        voluntary_exits: body.voluntary_exits.clone(),
                        sync_aggregate: body.sync_aggregate.clone(),
                        execution_payload: execution_payload.into(),
                        bls_to_execution_changes: body.bls_to_execution_changes.clone(),
                        blob_kzg_commitments: body.blob_kzg_commitments.clone(),
                        execution_requests: body.execution_requests.clone(),
                    },
                },
                signature,
            };

            let signed_block = SignedBeaconBlock::Electra(inner).into();

            Ok(VersionedSignedProposal {
                signed_block,
                kzg_proofs: blobs_bundle.proofs.clone(),
                blobs: blobs_bundle.blobs.clone(),
            })
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PreferencesHeader {
    /// Deprecated: This field is maintained for backward compatibility.
    pub censoring: Option<bool>,

    /// Allows validators to indicate whether global or regional filtering should be applied.
    pub filtering: Option<Filtering>,

    /// An optional list of BuilderIDs. If this is set, the relay will only accept
    /// submissions from builders whose public keys are linked to the IDs in this list.
    /// This allows for limiting submissions to a trusted set of builders.
    pub trusted_builders: Option<Vec<String>>,

    /// Allows validators to express a preference for whether a delay should be applied to get
    /// headers or not.
    pub header_delay: Option<bool>,
}
