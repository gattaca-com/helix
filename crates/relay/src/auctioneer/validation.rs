use alloy_primitives::B256;
use flux_profiler::timed;
use helix_common::{BuilderInfo, ForkKey, PayloadAttributesUpdate, is_local_dev};
use helix_types::{BlockValidationError, ForkName, SignedBidSubmission, Submission};

use crate::auctioneer::{
    bid_adjustor::BidAdjustor,
    context::Context,
    types::{SlotData, SubmissionData},
};

impl<B: BidAdjustor> Context<B> {
    #[timed]
    pub fn validate_submission<'a>(
        &mut self,
        submission_data: &SubmissionData,
        builder_info: &BuilderInfo,
        slot_data: &'a SlotData,
    ) -> Result<&'a PayloadAttributesUpdate, BlockValidationError> {
        let submission = &submission_data.submission;

        if submission.slot() != self.bid_slot {
            return Err(BlockValidationError::SubmissionForWrongSlot {
                expected: self.bid_slot,
                got: submission.slot(),
            });
        }

        // The relay never recomputes a block hash, so a stored hash only vouches for the bid
        // trace it was stored with; a resubmission of the same block carries the same trace.
        // The previous Gloas generation counts too, since its payloads are still revealed.
        let bid_trace = submission.bid_trace();
        let trace_mismatch = match submission {
            Submission::Full(SignedBidSubmission::Gloas(_)) => self
                .gloas_payloads
                .get_for_reveal(&bid_trace.block_hash)
                .is_some_and(|stored| !stored.has_bid_trace(bid_trace)),
            _ => self
                .payloads
                .get(&bid_trace.block_hash)
                .is_some_and(|stored| !stored.has_bid_trace(bid_trace)),
        };
        if trace_mismatch {
            return Err(BlockValidationError::BidTraceMismatch { block_hash: bid_trace.block_hash });
        }

        let payload_attributes = match submission {
            Submission::Full(SignedBidSubmission::Gloas(gloas)) => {
                slot_data.payload_attributes_map.get(&ForkKey {
                    parent_hash: *submission.parent_hash(),
                    parent_root: gloas.message.parent_block_root,
                })
            }
            _ => slot_data.attrs_for_submission(submission.parent_hash(), submission.prev_randao()),
        };
        let Some(payload_attributes) = payload_attributes else {
            return Err(BlockValidationError::UknnownParentHash {
                submission: *submission.parent_hash(),
                have: slot_data.payload_attributes_map.keys().map(|k| k.parent_hash).collect(),
            });
        };

        // Keyed withdrawals are not on the wire, so the decoder leaves their
        // root to the hydration cache.
        let mut withdrawals_root = submission_data.withdrawals_root;
        if let helix_types::Submission::Dehydrated(ref dehydrated) = *submission {
            if !self.hydration_cache.can_hydrate(dehydrated, self.chain_info.max_blobs_per_block())
            {
                return Err(BlockValidationError::CannotHydrate);
            }
            withdrawals_root = withdrawals_root
                .or_else(|| self.hydration_cache.cached_withdrawals_root(dehydrated));
        }

        let slot = self.bid_slot.as_u64();
        self.cache.check_submission_version(
            submission.builder_pubkey(),
            slot,
            submission_data.version,
        )?;
        self.cache
            .submission_versions
            .insert(*submission.builder_pubkey(), (slot, submission_data.version));
        self.validate_submission_data(
            submission,
            submission_data.decoder_params.fork_name,
            withdrawals_root,
            slot_data,
            payload_attributes,
        )?;
        check_if_trusted_builder(builder_info, slot_data)?;

        Ok(payload_attributes)
    }

    #[timed]
    fn validate_submission_data(
        &self,
        payload: &Submission,
        decoded_fork: ForkName,
        withdrawals_root: Option<B256>,
        slot_data: &SlotData,
        payload_attributes: &PayloadAttributesUpdate,
    ) -> Result<(), BlockValidationError> {
        check_submission_fork(decoded_fork, slot_data.current_fork)?;

        // checks internal consistency of the payload
        payload.validate()?;

        if payload_attributes.timestamp != payload.timestamp() {
            return Err(BlockValidationError::IncorrectTimestamp {
                got: payload.timestamp(),
                expected: payload_attributes.timestamp,
            });
        }

        let registration = &slot_data.registration_data.entry.registration.message;
        // Local dev fabricates registrations without the real fee recipient.
        if !is_local_dev() && registration.fee_recipient != *payload.proposer_fee_recipient() {
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

        if withdrawals_root != Some(payload_attributes.withdrawals_root) {
            return Err(BlockValidationError::WithdrawalsRootMismatch {
                got: withdrawals_root.unwrap_or_default(),
                expected: payload_attributes.withdrawals_root,
            });
        }

        Ok(())
    }
}

pub fn check_if_trusted_builder(
    builder_info: &BuilderInfo,
    slot_data: &SlotData,
) -> Result<(), BlockValidationError> {
    if let Some(trusted_builders) = &slot_data.registration_data.entry.preferences.trusted_builders
    {
        if trusted_builders.is_empty() {
            return Ok(());
        }

        let mut builder_ids = builder_info
            .builder_id
            .iter()
            .chain(builder_info.builder_ids.iter().flat_map(|ids| ids.iter()));

        if builder_ids.any(|id| trusted_builders.contains(id)) {
            Ok(())
        } else {
            Err(BlockValidationError::BuilderNotInProposersTrustedList {
                proposer_trusted_builders: trusted_builders.clone(),
            })
        }
    } else {
        Ok(())
    }
}

/// The decoder records the fork it decoded with; a submission shaped for another fork cannot be
/// validated against this slot.
pub(super) fn check_submission_fork(
    decoded: ForkName,
    slot_fork: ForkName,
) -> Result<(), BlockValidationError> {
    if decoded != slot_fork {
        return Err(BlockValidationError::InvalidPayloadType { fork_name: slot_fork });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use helix_common::{
        BuilderInfo, ValidatorPreferences,
        api::{
            builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
        },
    };
    use helix_types::{BlockValidationError, ForkName};

    use crate::auctioneer::{
        types::SlotData,
        validation::{check_if_trusted_builder, check_submission_fork},
    };

    #[test]
    fn test_check_if_trusted_builder_empty_list() {
        let builder_info = BuilderInfo {
            builder_id: Some("not_trusted".to_string()),
            builder_ids: None,
            ..Default::default()
        };
        let slot_data = SlotData {
            bid_slot: Default::default(),
            registration_data: Default::default(),
            current_fork: ForkName::Fulu,
            payload_attributes_map: Default::default(),
            il: Default::default(),
        };
        assert!(check_if_trusted_builder(&builder_info, &slot_data).is_ok());
    }

    #[test]
    fn test_check_if_trusted_builder_id_in_list() {
        let builder_info = BuilderInfo {
            builder_id: Some("trusted".to_string()),
            builder_ids: None,
            ..Default::default()
        };
        let slot_data = SlotData {
            bid_slot: Default::default(),
            registration_data: BuilderGetValidatorsResponseEntry {
                entry: ValidatorRegistrationInfo {
                    preferences: ValidatorPreferences {
                        trusted_builders: Some(vec!["trusted".to_string()]),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            current_fork: ForkName::Fulu,
            payload_attributes_map: Default::default(),
            il: Default::default(),
        };

        assert!(check_if_trusted_builder(&builder_info, &slot_data).is_ok());
    }

    #[test]
    fn test_check_if_trusted_builder_ids_in_list() {
        let builder_info = BuilderInfo {
            builder_id: Some("not_trusted".to_string()),
            builder_ids: Some(vec!["trusted".to_string()]),
            ..Default::default()
        };
        let slot_data = SlotData {
            bid_slot: Default::default(),
            registration_data: BuilderGetValidatorsResponseEntry {
                entry: ValidatorRegistrationInfo {
                    preferences: ValidatorPreferences {
                        trusted_builders: Some(vec!["trusted".to_string()]),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            current_fork: ForkName::Fulu,
            payload_attributes_map: Default::default(),
            il: Default::default(),
        };

        assert!(check_if_trusted_builder(&builder_info, &slot_data).is_ok());
    }

    #[test]
    fn test_check_if_trusted_builder_not_in_list() {
        let builder_info = BuilderInfo {
            builder_id: Some("not_trusted".to_string()),
            builder_ids: None,
            ..Default::default()
        };
        let slot_data = SlotData {
            bid_slot: Default::default(),
            registration_data: BuilderGetValidatorsResponseEntry {
                entry: ValidatorRegistrationInfo {
                    preferences: ValidatorPreferences {
                        trusted_builders: Some(vec!["trusted".to_string()]),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            current_fork: ForkName::Fulu,
            payload_attributes_map: Default::default(),
            il: Default::default(),
        };

        assert!(check_if_trusted_builder(&builder_info, &slot_data).is_err());
    }

    #[test]
    fn a_gloas_submission_passes_the_fork_gate_on_a_gloas_slot() {
        assert!(
            check_submission_fork(ForkName::Gloas, ForkName::Gloas).is_ok(),
            "the decoder decoded a Gloas submission for a Gloas slot",
        );
    }

    #[test]
    fn a_submission_decoded_for_another_fork_is_refused() {
        let result = check_submission_fork(ForkName::Fulu, ForkName::Gloas);

        assert!(
            matches!(
                result,
                Err(BlockValidationError::InvalidPayloadType { fork_name: ForkName::Gloas })
            ),
            "the slot's fork is the one the submission had to match",
        );
    }
}
