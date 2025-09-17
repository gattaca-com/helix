use alloy_primitives::B256;
use helix_common::{bid_submission::BidSubmission, BuilderInfo};
use helix_types::BlsPublicKeyBytes;

use crate::builder::{
    error::BuilderApiError,
    simulator_2::{NewSubmission, SlotContext, SortingData},
};

impl SortingData {
    pub fn validate_submission(
        &mut self,
        submission: &NewSubmission,
        builder_info: &BuilderInfo,
    ) -> Result<(), BuilderApiError> {
        // slot
        if submission.slot() != self.slot.bid_slot {
            return Err(BuilderApiError::SubmissionForWrongSlot {
                expected: self.slot.bid_slot,
                got: submission.slot(),
            });
        }

        // sequence
        if let Some(seq) = submission.sequence {
            self.check_and_update_sequence_number(submission.builder_public_key(), seq)?;
        }

        // duplicates
        self.check_duplicate_submission(*submission.block_hash())?;

        // TODO: last slot delivered in get_payload state
        self.validate_submission_data(&submission.payload, &submission.withdrawals_root)?;

        // trusted builder
        if let Some(trusted_builders) =
            self.slot.registration_data.entry.preferences.trusted_builders.as_ref()
        {
            let mut untrusted = false;
            if let Some(builder_id) = &builder_info.builder_id {
                if !trusted_builders.contains(builder_id) {
                    untrusted = true;
                };
            } else if let Some(ids) = &builder_info.builder_ids {
                if ids.iter().all(|id| !trusted_builders.contains(id)) {
                    untrusted = true;
                }
            } else {
                untrusted = true
            }

            if untrusted {
                return Err(BuilderApiError::BuilderNotInProposersTrustedList {
                    proposer_trusted_builders: vec![], // TODO
                })
            }
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
        seq: u64,
    ) -> Result<(), BuilderApiError> {
        todo!()
    }
}
