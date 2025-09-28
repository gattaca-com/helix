use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use helix_common::GetPayloadTrace;
use helix_types::{
    BeaconBlockBodyElectra, BeaconBlockElectra, GetPayloadResponse, PayloadAndBlobs,
    SignedBeaconBlock, SignedBeaconBlockElectra, SignedBidSubmission, SignedBlindedBeaconBlock,
    VersionedSignedProposal,
};
use tokio::sync::oneshot;

use crate::{
    builder::simulator_2::{
        worker::{GetPayloadResult, GetPayloadResultData},
        Context, PendingPayload, SortingData,
    },
    proposer::ProposerApiError,
    Api,
};

impl SortingData {
    pub(super) fn handle_get_payload<A: Api>(
        &mut self,
        block_hash: B256,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
        ctx: &mut Context<A>,
    ) {
        if let Some(payload) = self.payloads.get(&block_hash) {
            let res = self._get_payload(blinded, payload, trace).map(
                |(to_proposer, to_publish, trace)| {
                    let proposer_pubkey =
                        self.slot.registration_data.entry.registration.message.pubkey;

                    GetPayloadResultData {
                        to_proposer,
                        to_publish,
                        trace,
                        proposer_pubkey,
                        fork: self.slot.current_fork,
                    }
                },
            );

            let _ = res_tx.send(res);
        } else {
            // we may still receive the payload from builder / gossip, save request for
            // later
            ctx.pending_payloads = Some(PendingPayload {
                block_hash,
                blinded,
                res_tx,
                retry_at: Instant::now() + Duration::from_millis(20),
            });
        }
    }

    fn _get_payload(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: &SignedBidSubmission,
        trace: GetPayloadTrace,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal, GetPayloadTrace), ProposerApiError>
    {
        // TODO: use trace
        let (to_proposer, to_publish) = self.validate_and_unblind(blinded, local)?;
        Ok((to_proposer, to_publish, trace))
    }

    pub fn validate_and_unblind(
        &self,
        blinded: SignedBlindedBeaconBlock,
        local: &SignedBidSubmission,
    ) -> Result<(GetPayloadResponse, VersionedSignedProposal), ProposerApiError> {
        self.validate_proposal_coordinate(&blinded)?;

        if blinded.fork_name_unchecked() != self.slot.current_fork {
            return Err(ProposerApiError::UnsupportedBeaconChainVersion);
        }

        match blinded {
            SignedBlindedBeaconBlock::Altair(_) |
            SignedBlindedBeaconBlock::Base(_) |
            SignedBlindedBeaconBlock::Bellatrix(_) |
            SignedBlindedBeaconBlock::Capella(_) |
            SignedBlindedBeaconBlock::Deneb(_) |
            SignedBlindedBeaconBlock::Fulu(_) |
            SignedBlindedBeaconBlock::Gloas(_) => {
                Err(ProposerApiError::UnsupportedBeaconChainVersion)
            }
            SignedBlindedBeaconBlock::Electra(blinded_block) => {
                // validate
                // TODO: we should already have the header, as we served it in "get_header"
                // NOTE: not if it comes via gossip, just implement the check manually
                let local_header = local
                    .execution_payload_ref()
                    .to_header(None)
                    .to_lighthouse_electra_header()
                    .map_err(ProposerApiError::SszError)?;

                let block = &blinded_block.message;
                let body = &block.body;
                let provided_header = &body.execution_payload.execution_payload_header;

                if &local_header != provided_header {
                    return Err(ProposerApiError::BlindedBlockAndPayloadHeaderMismatch);
                }

                let local_kzg_commitments = &local.blobs_bundle().commitments;

                if !local_kzg_commitments.iter().eq(body.blob_kzg_commitments.iter().map(|p| p.0)) {
                    return Err(ProposerApiError::BlobKzgCommitmentsMismatch);
                }

                // unblind
                let signature = blinded_block.signature.clone();

                let execution_payload = local
                    .execution_payload_ref()
                    .to_lighthouse_electra_paylaod()
                    .map_err(ProposerApiError::SszError)?;

                let blobs_bundle = local.blobs_bundle();

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

                // TODO: avoid this endless cloning
                let signed_block = SignedBeaconBlock::Electra(inner).into();
                let to_broadcast = VersionedSignedProposal {
                    signed_block,
                    kzg_proofs: blobs_bundle.proofs.clone(),
                    blobs: blobs_bundle.blobs.clone(),
                };
                let to_proposer = GetPayloadResponse {
                    version: self.slot.current_fork,
                    metadata: Default::default(),
                    data: Arc::new(PayloadAndBlobs {
                        execution_payload: local.execution_payload_ref().clone(),
                        blobs_bundle: Arc::unwrap_or_clone(blobs_bundle),
                    }),
                };

                Ok((to_proposer, to_broadcast))
            }
        }
    }

    fn validate_proposal_coordinate(
        &self,
        blinded: &SignedBlindedBeaconBlock,
    ) -> Result<(), ProposerApiError> {
        let slot_duty = &self.slot.registration_data;
        let actual_index = blinded.message().proposer_index();
        let expected_index = slot_duty.validator_index;

        if expected_index != actual_index {
            return Err(ProposerApiError::UnexpectedProposerIndex {
                expected: expected_index,
                actual: actual_index,
            });
        }

        if self.slot.bid_slot != slot_duty.slot {
            return Err(ProposerApiError::InternalSlotMismatchesWithSlotDuty {
                internal_slot: self.slot.bid_slot,
                slot_duty_slot: slot_duty.slot,
            });
        }

        if slot_duty.slot != blinded.message().slot() {
            return Err(ProposerApiError::InvalidBlindedBlockSlot {
                internal_slot: slot_duty.slot,
                blinded_block_slot: blinded.message().slot(),
            });
        }

        Ok(())
    }
}
