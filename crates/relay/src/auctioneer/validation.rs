use alloy_primitives::B256;
use helix_common::BuilderInfo;
use helix_types::{
    BlockValidationError, BlsPublicKeyBytes, SignedBidSubmission, SubmissionVersion,
};

use crate::{
    auctioneer::{bid_adjustor::BidAdjustor, context::Context, types::SlotData},
    housekeeper::PayloadAttributesUpdate,
};

impl<B: BidAdjustor> Context<B> {
    pub fn validate_submission<'a>(
        &mut self,
        submission: &SignedBidSubmission,
        version: SubmissionVersion,
        withdrawals_root: &B256,
        builder_info: &BuilderInfo,
        slot_data: &'a SlotData,
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

        self.staleness_check(submission.builder_public_key(), version)?;
        self.validate_submission_data(submission, withdrawals_root, slot_data, payload_attributes)?;
        check_if_trusted_builder(builder_info, slot_data)?;

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

    fn staleness_check(
        &mut self,
        builder: &BlsPublicKeyBytes,
        version: SubmissionVersion,
    ) -> Result<(), BlockValidationError> {
        if let Some(old_version) = self.version.get_mut(builder) {
            if *old_version >= version {
                return Err(BlockValidationError::OutOfSequence {
                    seen: *old_version,
                    this: version,
                });
            } else {
                *old_version = version
            }
        } else {
            self.version.insert(*builder, version);
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

#[cfg(test)]
mod tests {
    use helix_common::{
        BuilderInfo, ValidatorPreferences,
        api::{
            builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
        },
    };
    use helix_types::ForkName;

    use crate::auctioneer::{types::SlotData, validation::check_if_trusted_builder};

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
}
