use alloy_primitives::B256;
use helix_common::BuilderInfo;
use helix_types::{BlockValidationError, BlsPublicKeyBytes, SignedBidSubmission};

use crate::{
    auctioneer::{context::Context, types::SlotData},
    housekeeper::PayloadAttributesUpdate,
};

impl Context {
    pub fn validate_submission<'a>(
        &mut self,
        submission: &SignedBidSubmission,
        withdrawals_root: &B256,
        sequence: Option<u64>,
        builder_info: &BuilderInfo,
        slot_data: &'a SlotData,
        on_receive_ns: u64,
    ) -> Result<&'a PayloadAttributesUpdate, BlockValidationError> {
        if submission.slot() != self.bid_slot {
            return Err(BlockValidationError::SubmissionForWrongSlot {
                expected: self.bid_slot,
                got: submission.slot(),
            });
        }

        let Some(payload_attributes) =
            slot_data.payload_attributes_map.get(submission.parent_hash())
        else {
            return Err(BlockValidationError::UknnownParentHash {
                submission: *submission.parent_hash(),
                have: slot_data.payload_attributes_map.keys().cloned().collect(),
            });
        };

        self.staleness_check(submission.builder_public_key(), on_receive_ns, sequence)?;
        self.check_duplicate_submission(*submission.block_hash())?;
        self.validate_submission_data(submission, withdrawals_root, slot_data, payload_attributes)?;
        self.check_if_trusted_builder(builder_info, slot_data)?;

        Ok(payload_attributes)
    }

    fn validate_submission_data(
        &self,
        payload: &SignedBidSubmission,
        withdrawals_root: &B256,
        slot_data: &SlotData,
        payload_attributes: &PayloadAttributesUpdate,
    ) -> Result<(), BlockValidationError> {
        if slot_data.current_fork != payload.fork_name() {
            return Err(BlockValidationError::InvalidPayloadType {
                fork_name: slot_data.current_fork,
            });
        }

        // checks internal consistency of the payload
        payload.validate()?;

        if payload_attributes.timestamp != payload.timestamp() {
            return Err(BlockValidationError::IncorrectTimestamp {
                got: payload.timestamp(),
                expected: payload_attributes.timestamp,
            });
        }

        let registration = &slot_data.registration_data.entry.registration.message;
        if registration.fee_recipient != *payload.proposer_fee_recipient() {
            return Err(BlockValidationError::FeeRecipientMismatch {
                got: *payload.proposer_fee_recipient(),
                expected: registration.fee_recipient,
            });
        }

        let bid_trace = payload.bid_trace();
        if registration.pubkey != bid_trace.proposer_pubkey {
            return Err(BlockValidationError::ProposerPublicKeyMismatch {
                got: bid_trace.proposer_pubkey,
                expected: registration.pubkey,
            });
        }

        if *payload.prev_randao() != payload_attributes.prev_randao {
            return Err(BlockValidationError::PrevRandaoMismatch {
                got: *payload.prev_randao(),
                expected: payload_attributes.prev_randao,
            });
        }

        if *withdrawals_root != payload_attributes.withdrawals_root {
            return Err(BlockValidationError::WithdrawalsRootMismatch {
                got: *withdrawals_root,
                expected: payload_attributes.withdrawals_root,
            });
        }

        Ok(())
    }

    fn check_duplicate_submission(&mut self, block_hash: B256) -> Result<(), BlockValidationError> {
        if !self.seen_block_hashes.insert(block_hash) {
            return Err(BlockValidationError::DuplicateBlockHash { block_hash });
        }

        Ok(())
    }

    fn staleness_check(
        &mut self,
        builder: &BlsPublicKeyBytes,
        new_receive_ns: u64,
        new_seq: Option<u64>,
    ) -> Result<(), BlockValidationError> {
        if let Some((old_receive_ns, maybe_old_seq)) = self.sequence.get_mut(builder) {
            let mut check_timestamp = true;

            match (&maybe_old_seq, new_seq) {
                (None, None) | (Some(_), None) => (),
                (None, Some(new_seq)) => {
                    *maybe_old_seq = Some(new_seq);
                    check_timestamp = false
                }
                (Some(old_seq), Some(new_seq)) => {
                    if new_seq > *old_seq {
                        *maybe_old_seq = Some(new_seq);
                        check_timestamp = false;
                    } else {
                        return Err(BlockValidationError::OutOfSequence {
                            seen: *old_seq,
                            this: new_seq,
                        });
                    }
                }
            }

            if check_timestamp {
                if new_receive_ns > *old_receive_ns {
                    *old_receive_ns = new_receive_ns
                } else {
                    return Err(BlockValidationError::AlreadyProcessingNewerPayload);
                }
            }
        } else {
            self.sequence.insert(*builder, (new_receive_ns, new_seq));
        }

        Ok(())
    }

    fn check_if_trusted_builder(
        &self,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
    ) -> Result<(), BlockValidationError> {
        if let Some(trusted_builders) =
            &slot_data.registration_data.entry.preferences.trusted_builders
        {
            // Handle case where proposer specifies an empty list.
            if trusted_builders.is_empty() {
                return Ok(());
            }

            if let Some(builder_id) = &builder_info.builder_id {
                if trusted_builders.contains(builder_id) {
                    Ok(())
                } else {
                    Err(BlockValidationError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else if let Some(ids) = &builder_info.builder_ids {
                if ids.iter().any(|id| trusted_builders.contains(id)) {
                    Ok(())
                } else {
                    Err(BlockValidationError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else {
                Err(BlockValidationError::BuilderNotInProposersTrustedList {
                    proposer_trusted_builders: trusted_builders.clone(),
                })
            }
        } else {
            Ok(())
        }
    }
}
