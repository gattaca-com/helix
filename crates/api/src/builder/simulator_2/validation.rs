use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{bid_submission::BidSubmission, BuilderInfo};
use helix_types::{
    BeaconBlockBodyElectra, BeaconBlockElectra, BlindedPayloadRef, BlsPublicKeyBytes,
    GetPayloadResponse, PayloadAndBlobs, SignedBeaconBlock, SignedBeaconBlockElectra,
    SignedBidSubmission, SignedBlindedBeaconBlock, VersionedSignedProposal,
};

use crate::{
    builder::{
        error::BuilderApiError,
        simulator_2::{SlotContext, SortingData},
    },
    proposer::ProposerApiError,
};

// TODO: metadata

impl SortingData {
    pub fn validate_submission(
        &mut self,
        payload: &SignedBidSubmission,
        withdrawals_root: &B256,
        sequence: Option<u64>,
        builder_info: &BuilderInfo,
    ) -> Result<(), BuilderApiError> {
        // slot
        if payload.slot() != self.slot.bid_slot {
            return Err(BuilderApiError::SubmissionForWrongSlot {
                expected: self.slot.bid_slot,
                got: payload.slot(),
            });
        }

        // sequence
        if let Some(seq) = sequence {
            self.check_and_update_sequence_number(payload.builder_public_key(), seq)?;
        }

        // duplicates
        self.check_duplicate_submission(*payload.block_hash())?;

        // TODO: last slot delivered in get_payload state
        self.validate_submission_data(payload, &withdrawals_root)?;

        // trusted builder
        if self.check_if_trusted_builder(payload, builder_info) {
            return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders: vec![], // TODO
            })
        }

        Ok(())
    }

    fn validate_submission_data(
        &self,
        payload: &impl BidSubmission,
        withdrawals_root: &B256,
    ) -> Result<(), BuilderApiError> {
        if self.slot.current_fork != payload.fork_name() {
            return Err(BuilderApiError::InvalidPayloadType { fork_name: self.slot.current_fork });
        }

        // checks internal consistency of the payload
        payload.validate()?;

        if self.slot.payload_attributes.payload_attributes.timestamp != payload.timestamp() {
            return Err(BuilderApiError::IncorrectTimestamp {
                got: payload.timestamp(),
                expected: self.slot.payload_attributes.payload_attributes.timestamp,
            });
        }

        let registration = &self.slot.registration_data.entry.registration.message;
        if registration.fee_recipient != *payload.proposer_fee_recipient() {
            return Err(BuilderApiError::FeeRecipientMismatch {
                got: *payload.proposer_fee_recipient(),
                expected: registration.fee_recipient,
            });
        }

        let bid_trace = payload.bid_trace();
        if registration.pubkey != bid_trace.proposer_pubkey {
            return Err(BuilderApiError::ProposerPublicKeyMismatch {
                got: bid_trace.proposer_pubkey,
                expected: registration.pubkey,
            });
        }

        let payload_attributes = &self.slot.payload_attributes;
        if *payload.prev_randao() != payload_attributes.prev_randao {
            return Err(BuilderApiError::PrevRandaoMismatch {
                got: *payload.prev_randao(),
                expected: payload_attributes.prev_randao,
            });
        }

        if *withdrawals_root != payload_attributes.withdrawals_root {
            return Err(BuilderApiError::WithdrawalsRootMismatch {
                got: *withdrawals_root,
                expected: payload_attributes.withdrawals_root,
            });
        }

        Ok(())
    }

    fn check_duplicate_submission(&mut self, block_hash: B256) -> Result<(), BuilderApiError> {
        if !self.seen_block_hashes.insert(block_hash) {
            return Err(BuilderApiError::DuplicateBlockHash { block_hash })
        }

        Ok(())
    }

    fn check_and_update_sequence_number(
        &mut self,
        builder: &BlsPublicKeyBytes,
        new_seq: u64,
    ) -> Result<(), BuilderApiError> {
        if let Some(mut old_seq) = self.sequence.get_mut(builder) {
            if new_seq > *old_seq {
                // higher sequence number, update
                *old_seq = new_seq;
            } else {
                // stale or duplicated sequence number
                return Err(BuilderApiError::OutOfSequence { seen: *old_seq, this: new_seq });
            }
        } else {
            self.sequence.insert(*builder, new_seq);
        }

        Ok(())
    }

    fn check_if_trusted_builder(
        &self,
        submission: &SignedBidSubmission,
        builder_info: &BuilderInfo,
    ) -> bool {
        if let Some(trusted_builders) =
            &self.slot.registration_data.entry.preferences.trusted_builders
        {
            // Handle case where proposer specifies an empty list.
            if trusted_builders.is_empty() {
                return true;
            }

            if let Some(builder_id) = &builder_info.builder_id {
                trusted_builders.contains(builder_id)
            } else if let Some(ids) = &builder_info.builder_ids {
                ids.iter().any(|id| trusted_builders.contains(id))
            } else {
                false
            }
        } else {
            true
        }
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
