use helix_types::{BlsPublicKeyBytes, Validator};
use serde::{Deserialize, Serialize};

use crate::{api::proposer_api::ValidatorRegistrationInfo, utils::utcnow_ms};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ValidatorSummary {
    #[serde(with = "serde_utils::quoted_u64")]
    pub index: u64,
    #[serde(with = "serde_utils::quoted_u64")]
    pub balance: u64,
    pub status: ValidatorStatus,
    pub validator: Validator,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorStatus {
    PendingInitialized,
    PendingQueued,
    ActiveOngoing,
    ActiveExiting,
    ActiveSlashed,
    ExitedUnslashed,
    ExitedSlashed,
    WithdrawalPossible,
    WithdrawalDone,
    Active,
    Pending,
    Exited,
    Withdrawal,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SignedValidatorRegistrationEntry {
    pub registration_info: ValidatorRegistrationInfo,
    pub inserted_at: u64,
    pub pool_name: Option<String>,
    pub user_agent: Option<String>,
}

impl SignedValidatorRegistrationEntry {
    pub fn new(
        registration_info: ValidatorRegistrationInfo,
        pool_name: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self { registration_info, inserted_at: utcnow_ms(), pool_name, user_agent }
    }

    pub fn public_key(&self) -> &BlsPublicKeyBytes {
        &self.registration_info.registration.message.pubkey
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_types::ValidatorRegistrationData;
    use alloy_primitives::Address;

    fn create_test_registration_info() -> ValidatorRegistrationInfo {
        ValidatorRegistrationInfo {
            registration: helix_types::SignedValidatorRegistration {
                message: ValidatorRegistrationData {
                    fee_recipient: Address::ZERO,
                    gas_limit: 30_000_000,
                    timestamp: 1234567890,
                    pubkey: BlsPublicKeyBytes::default(),
                },
                signature: Default::default(),
            },
            preferences: Default::default(),
        }
    }

    #[test]
    fn test_signed_validator_registration_entry_new() {
        let registration_info = create_test_registration_info();
        let pool_name = Some("test_pool".to_string());
        let user_agent = Some("test_agent/1.0".to_string());

        let entry = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            pool_name.clone(),
            user_agent.clone(),
        );

        assert_eq!(entry.pool_name, pool_name);
        assert_eq!(entry.user_agent, user_agent);
        assert!(entry.inserted_at > 0);
        assert_eq!(entry.registration_info.registration.message.gas_limit, 30_000_000);
    }

    #[test]
    fn test_signed_validator_registration_entry_new_without_optional_fields() {
        let registration_info = create_test_registration_info();

        let entry = SignedValidatorRegistrationEntry::new(registration_info.clone(), None, None);

        assert_eq!(entry.pool_name, None);
        assert_eq!(entry.user_agent, None);
        assert!(entry.inserted_at > 0);
    }

    #[test]
    fn test_public_key_accessor() {
        let registration_info = create_test_registration_info();
        let entry = SignedValidatorRegistrationEntry::new(registration_info.clone(), None, None);

        let pubkey = entry.public_key();
        assert_eq!(pubkey, &registration_info.registration.message.pubkey);
    }

    #[test]
    fn test_inserted_at_is_current_time() {
        let registration_info = create_test_registration_info();
        let before = utcnow_ms();
        let entry = SignedValidatorRegistrationEntry::new(registration_info, None, None);
        let after = utcnow_ms();

        assert!(entry.inserted_at >= before);
        assert!(entry.inserted_at <= after);
    }

    #[test]
    fn test_validator_summary_serialization() {
        let summary = ValidatorSummary {
            index: 12345,
            balance: 32_000_000_000,
            status: ValidatorStatus::ActiveOngoing,
            validator: Validator {
                pubkey: BlsPublicKeyBytes::default(),
                withdrawal_credentials: Default::default(),
                effective_balance: 32_000_000_000,
                slashed: false,
                activation_eligibility_epoch: 0u64.into(),
                activation_epoch: 0u64.into(),
                exit_epoch: u64::MAX.into(),
                withdrawable_epoch: u64::MAX.into(),
            },
        };

        let serialized = serde_json::to_string(&summary).unwrap();
        let deserialized: ValidatorSummary = serde_json::from_str(&serialized).unwrap();

        assert_eq!(summary.index, deserialized.index);
        assert_eq!(summary.balance, deserialized.balance);
    }

    #[test]
    fn test_validator_status_variants() {
        // Test that all validator status variants can be created
        let statuses = vec![
            ValidatorStatus::PendingInitialized,
            ValidatorStatus::PendingQueued,
            ValidatorStatus::ActiveOngoing,
            ValidatorStatus::ActiveExiting,
            ValidatorStatus::ActiveSlashed,
            ValidatorStatus::ExitedUnslashed,
            ValidatorStatus::ExitedSlashed,
            ValidatorStatus::WithdrawalPossible,
            ValidatorStatus::WithdrawalDone,
            ValidatorStatus::Active,
            ValidatorStatus::Pending,
            ValidatorStatus::Exited,
            ValidatorStatus::Withdrawal,
        ];

        // All statuses should be created without panic
        assert_eq!(statuses.len(), 13);
    }

    #[test]
    fn test_validator_status_serialization() {
        let status = ValidatorStatus::ActiveOngoing;
        let serialized = serde_json::to_string(&status).unwrap();
        assert_eq!(serialized, "\"active_ongoing\"");

        let deserialized: ValidatorStatus = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(deserialized, ValidatorStatus::ActiveOngoing));
    }

    #[test]
    fn test_entry_with_different_pool_names() {
        let registration_info = create_test_registration_info();
        
        let entry1 = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some("pool_a".to_string()),
            None,
        );
        let entry2 = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some("pool_b".to_string()),
            None,
        );

        assert_ne!(entry1.pool_name, entry2.pool_name);
    }
}
