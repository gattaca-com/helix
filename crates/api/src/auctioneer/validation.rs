use alloy_primitives::B256;
use helix_common::{bid_submission::BidSubmission, BuilderInfo};
use helix_types::{BlsPublicKeyBytes, SignedBidSubmission};

use crate::{auctioneer::types::SortingData, builder::error::BuilderApiError};

// TODO: metadata

impl SortingData {
    pub fn validate_submission(
        &mut self,
        payload: &SignedBidSubmission,
        withdrawals_root: &B256,
        sequence: Option<u64>,
        builder_info: &BuilderInfo,
    ) -> Result<(), BuilderApiError> {
        if payload.slot() != self.slot.bid_slot {
            return Err(BuilderApiError::SubmissionForWrongSlot {
                expected: self.slot.bid_slot,
                got: payload.slot(),
            });
        }

        if let Some(seq) = sequence {
            self.check_and_update_sequence_number(payload.builder_public_key(), seq)?;
        }

        self.check_duplicate_submission(*payload.block_hash())?;
        self.validate_submission_data(payload, &withdrawals_root)?;
        self.check_if_trusted_builder(builder_info)?;

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
        if let Some(old_seq) = self.sequence.get_mut(builder) {
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

    fn check_if_trusted_builder(&self, builder_info: &BuilderInfo) -> Result<(), BuilderApiError> {
        if let Some(trusted_builders) =
            &self.slot.registration_data.entry.preferences.trusted_builders
        {
            // Handle case where proposer specifies an empty list.
            if trusted_builders.is_empty() {
                return Ok(());
            }

            if let Some(builder_id) = &builder_info.builder_id {
                if trusted_builders.contains(builder_id) {
                    Ok(())
                } else {
                    return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else if let Some(ids) = &builder_info.builder_ids {
                if ids.iter().any(|id| trusted_builders.contains(id)) {
                    Ok(())
                } else {
                    return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else {
                return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                    proposer_trusted_builders: trusted_builders.clone(),
                })
            }
        } else {
            Ok(())
        }
    }
}
