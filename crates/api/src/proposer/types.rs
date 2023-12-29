use ethereum_consensus::{
    bellatrix, capella, deneb,
    primitives::{BlsPublicKey, Hash32},
    types::mainnet::{ExecutionPayload, SignedBeaconBlock, SignedBlindedBeaconBlock},
};
use serde::Deserialize;

use crate::proposer::error::ProposerApiError;

pub const PATH_PROPOSER_API: &str = "/eth/v1/builder";

pub const PATH_STATUS: &str = "/status";
pub const PATH_REGISTER_VALIDATORS: &str = "/validators";
pub const PATH_GET_HEADER: &str = "/header/:slot/:parent_hash/:pubkey";
pub const PATH_GET_PAYLOAD: &str = "/blinded_blocks";

pub const GET_HEADER_REQUEST_CUTOFF_MS: i64 = 3000;

#[derive(Debug, Deserialize)]
pub struct GetHeaderParams {
    pub slot: u64,
    pub parent_hash: Hash32,
    #[serde(rename = "pubkey")]
    pub public_key: BlsPublicKey,
}

pub fn unblind_beacon_block(
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    execution_payload: &ExecutionPayload,
) -> Result<SignedBeaconBlock, ProposerApiError> {
    match signed_blinded_beacon_block {
        SignedBlindedBeaconBlock::Bellatrix(blinded_block) => {
            let signature = blinded_block.signature.clone();
            let block = &blinded_block.message;
            let body = &block.body;
            let execution_payload =
                execution_payload.bellatrix().ok_or(ProposerApiError::PayloadTypeMismatch)?;

            let inner = bellatrix::SignedBeaconBlock {
                message: bellatrix::BeaconBlock {
                    slot: block.slot,
                    proposer_index: block.proposer_index,
                    parent_root: block.parent_root,
                    state_root: block.state_root,
                    body: bellatrix::BeaconBlockBody {
                        randao_reveal: body.randao_reveal.clone(),
                        eth1_data: body.eth1_data.clone(),
                        graffiti: body.graffiti.clone(),
                        proposer_slashings: body.proposer_slashings.clone(),
                        attester_slashings: body.attester_slashings.clone(),
                        attestations: body.attestations.clone(),
                        deposits: body.deposits.clone(),
                        voluntary_exits: body.voluntary_exits.clone(),
                        sync_aggregate: body.sync_aggregate.clone(),
                        execution_payload: execution_payload.clone(),
                    },
                },
                signature,
            };
            Ok(SignedBeaconBlock::Bellatrix(inner))
        }
        SignedBlindedBeaconBlock::Capella(blinded_block) => {
            let signature = blinded_block.signature.clone();
            let block = &blinded_block.message;
            let body = &block.body;
            let execution_payload =
                execution_payload.capella().ok_or(ProposerApiError::PayloadTypeMismatch)?;

            let inner = capella::SignedBeaconBlock {
                message: capella::BeaconBlock {
                    slot: block.slot,
                    proposer_index: block.proposer_index,
                    parent_root: block.parent_root,
                    state_root: block.state_root,
                    body: capella::BeaconBlockBody {
                        randao_reveal: body.randao_reveal.clone(),
                        eth1_data: body.eth1_data.clone(),
                        graffiti: body.graffiti.clone(),
                        proposer_slashings: body.proposer_slashings.clone(),
                        attester_slashings: body.attester_slashings.clone(),
                        attestations: body.attestations.clone(),
                        deposits: body.deposits.clone(),
                        voluntary_exits: body.voluntary_exits.clone(),
                        sync_aggregate: body.sync_aggregate.clone(),
                        execution_payload: execution_payload.clone(),
                        bls_to_execution_changes: body.bls_to_execution_changes.clone(),
                    },
                },
                signature,
            };
            Ok(SignedBeaconBlock::Capella(inner))
        }
        SignedBlindedBeaconBlock::Deneb(blinded_block) => {
            let signature = blinded_block.signature.clone();
            let block = &blinded_block.message;
            let body = &block.body;
            let execution_payload =
                execution_payload.deneb().ok_or(ProposerApiError::PayloadTypeMismatch)?;

            let inner = deneb::SignedBeaconBlock {
                message: deneb::BeaconBlock {
                    slot: block.slot,
                    proposer_index: block.proposer_index,
                    parent_root: block.parent_root,
                    state_root: block.state_root,
                    body: deneb::BeaconBlockBody {
                        randao_reveal: body.randao_reveal.clone(),
                        eth1_data: body.eth1_data.clone(),
                        graffiti: body.graffiti.clone(),
                        proposer_slashings: body.proposer_slashings.clone(),
                        attester_slashings: body.attester_slashings.clone(),
                        attestations: body.attestations.clone(),
                        deposits: body.deposits.clone(),
                        voluntary_exits: body.voluntary_exits.clone(),
                        sync_aggregate: body.sync_aggregate.clone(),
                        execution_payload: execution_payload.clone(),
                        bls_to_execution_changes: body.bls_to_execution_changes.clone(),
                        blob_kzg_commitments: body.blob_kzg_commitments.clone(),
                    },
                },
                signature,
            };
            Ok(SignedBeaconBlock::Deneb(inner))
        }
    }
}
