use alloy_primitives::B256;
use helix_common::BuilderInfo;
use helix_types::{BlsPublicKeyBytes, SignedBidSubmission};

use crate::{
    auctioneer::{context::Context, types::SlotData},
    builder::error::BuilderApiError,
    Api,
};

impl<A: Api> Context<A> {
    pub fn validate_submission(
        &mut self,
        payload: &SignedBidSubmission,
        withdrawals_root: &B256,
        sequence: Option<u64>,
        builder_info: &BuilderInfo,
        slot_data: &SlotData,
        on_receive_ns: u64,
    ) -> Result<(), BuilderApiError> {
        if payload.slot() != self.bid_slot {
            return Err(BuilderApiError::SubmissionForWrongSlot {
                expected: self.bid_slot,
                got: payload.slot(),
            });
        }

        self.staleness_check(payload.builder_public_key(), on_receive_ns, sequence)?;
        self.check_duplicate_submission(*payload.block_hash())?;
        self.validate_submission_data(payload, withdrawals_root, slot_data)?;
        self.check_if_trusted_builder(builder_info, slot_data)?;

        Ok(())
    }

    fn validate_submission_data(
        &self,
        payload: &SignedBidSubmission,
        withdrawals_root: &B256,
        slot_data: &SlotData,
    ) -> Result<(), BuilderApiError> {
        if slot_data.current_fork != payload.fork_name() {
            return Err(BuilderApiError::InvalidPayloadType { fork_name: slot_data.current_fork });
        }

        // checks internal consistency of the payload
        payload.validate()?;

        if slot_data.payload_attributes.payload_attributes.timestamp != payload.timestamp() {
            return Err(BuilderApiError::IncorrectTimestamp {
                got: payload.timestamp(),
                expected: slot_data.payload_attributes.payload_attributes.timestamp,
            });
        }

        let registration = &slot_data.registration_data.entry.registration.message;
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

        let payload_attributes = &slot_data.payload_attributes;
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
            return Err(BuilderApiError::DuplicateBlockHash { block_hash });
        }

        Ok(())
    }

    fn staleness_check(
        &mut self,
        builder: &BlsPublicKeyBytes,
        new_receive_ns: u64,
        new_seq: Option<u64>,
    ) -> Result<(), BuilderApiError> {
        if let Some((old_receive_ns, maybe_old_seq)) = self.sequence.get_mut(builder) {
            if new_receive_ns > *old_receive_ns {
                *old_receive_ns = new_receive_ns
            } else {
                return Err(BuilderApiError::AlreadyProcessingNewerPayload);
            }

            match (&maybe_old_seq, new_seq) {
                (None, None) | (Some(_), None) => (),
                (None, Some(new_seq)) => *maybe_old_seq = Some(new_seq),
                (Some(old_seq), Some(new_seq)) => {
                    if new_seq > *old_seq {
                        *maybe_old_seq = Some(new_seq);
                    } else {
                        return Err(BuilderApiError::OutOfSequence {
                            seen: *old_seq,
                            this: new_seq,
                        });
                    }
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
    ) -> Result<(), BuilderApiError> {
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
                    Err(BuilderApiError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else if let Some(ids) = &builder_info.builder_ids {
                if ids.iter().any(|id| trusted_builders.contains(id)) {
                    Ok(())
                } else {
                    Err(BuilderApiError::BuilderNotInProposersTrustedList {
                        proposer_trusted_builders: trusted_builders.clone(),
                    })
                }
            } else {
                Err(BuilderApiError::BuilderNotInProposersTrustedList {
                    proposer_trusted_builders: trusted_builders.clone(),
                })
            }
        } else {
            Ok(())
        }
    }
}
