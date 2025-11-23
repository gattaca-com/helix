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

    #[test]
    fn test_pool_name_and_user_agent_edge_cases() {
        let registration_info = create_test_registration_info();
        
        // Empty strings (different from None)
        let empty = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some(String::new()),
            Some(String::new()),
        );
        assert_eq!(empty.pool_name, Some(String::new()));
        assert_eq!(empty.user_agent, Some(String::new()));

        // Very long strings
        let long_pool = "a".repeat(10_000);
        let long_agent = "b".repeat(10_000);
        let long = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some(long_pool.clone()),
            Some(long_agent.clone()),
        );
        assert_eq!(long.pool_name.as_ref().unwrap().len(), 10_000);
        assert_eq!(long.user_agent.as_ref().unwrap().len(), 10_000);

        // Special characters and Unicode
        let special = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some("Pool-123_TEST!@#$%^&*() 🌊".to_string()),
            Some("Mozilla/5.0 (池; 日本語) Agent/2.0".to_string()),
        );
        assert!(special.pool_name.unwrap().contains("🌊"));
        assert!(special.user_agent.unwrap().contains("日本語"));

        // Whitespace-only strings
        let whitespace = SignedValidatorRegistrationEntry::new(
            registration_info.clone(),
            Some("   \t\n  ".to_string()),
            Some("   ".to_string()),
        );
        assert_eq!(whitespace.pool_name, Some("   \t\n  ".to_string()));
    }

    #[test]
    fn test_inserted_at_timestamps_are_monotonic() {
        let registration_info = create_test_registration_info();
        
        // Create multiple entries rapidly
        let mut entries = Vec::new();
        for _ in 0..10 {
            entries.push(SignedValidatorRegistrationEntry::new(
                registration_info.clone(),
                None,
                None,
            ));
        }

        // Timestamps should be non-decreasing (monotonic)
        for i in 1..entries.len() {
            assert!(
                entries[i].inserted_at >= entries[i - 1].inserted_at,
                "Timestamps should be monotonically increasing: {} >= {}",
                entries[i].inserted_at,
                entries[i - 1].inserted_at
            );
        }

        // All should be positive
        for entry in &entries {
            assert!(entry.inserted_at > 0, "Timestamp should be positive");
        }
    }

    #[test]
    fn test_validator_status_serialization_all_variants() {
        // Test serialization format for ALL status variants
        let test_cases = vec![
            (ValidatorStatus::PendingInitialized, "\"pending_initialized\""),
            (ValidatorStatus::PendingQueued, "\"pending_queued\""),
            (ValidatorStatus::ActiveOngoing, "\"active_ongoing\""),
            (ValidatorStatus::ActiveExiting, "\"active_exiting\""),
            (ValidatorStatus::ActiveSlashed, "\"active_slashed\""),
            (ValidatorStatus::ExitedUnslashed, "\"exited_unslashed\""),
            (ValidatorStatus::ExitedSlashed, "\"exited_slashed\""),
            (ValidatorStatus::WithdrawalPossible, "\"withdrawal_possible\""),
            (ValidatorStatus::WithdrawalDone, "\"withdrawal_done\""),
            (ValidatorStatus::Active, "\"active\""),
            (ValidatorStatus::Pending, "\"pending\""),
            (ValidatorStatus::Exited, "\"exited\""),
            (ValidatorStatus::Withdrawal, "\"withdrawal\""),
        ];

        for (status, expected_json) in test_cases {
            let serialized = serde_json::to_string(&status).unwrap();
            assert_eq!(serialized, expected_json, "Failed for {:?}", status);
            
            // Test round-trip
            let deserialized: ValidatorStatus = serde_json::from_str(&serialized).unwrap();
            let reserialized = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(serialized, reserialized, "Round-trip failed for {:?}", status);
        }
    }

    #[test]
    fn test_validator_summary_index_and_balance_as_quoted_strings() {
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

        let json = serde_json::to_string(&summary).unwrap();
        
        // Should use quoted strings for u64 values (JavaScript compatibility)
        assert!(json.contains("\"index\":\"12345\""), "Index should be quoted");
        assert!(json.contains("\"balance\":\"32000000000\""), "Balance should be quoted");

        // Round-trip should work
        let deserialized: ValidatorSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.index, 12345);
        assert_eq!(deserialized.balance, 32_000_000_000);
    }

    #[test]
    fn test_validator_summary_edge_case_values() {
        // Test with maximum u64 values
        let max_summary = ValidatorSummary {
            index: u64::MAX,
            balance: u64::MAX,
            status: ValidatorStatus::WithdrawalDone,
            validator: Validator {
                pubkey: BlsPublicKeyBytes::default(),
                withdrawal_credentials: Default::default(),
                effective_balance: u64::MAX,
                slashed: true,
                activation_eligibility_epoch: u64::MAX.into(),
                activation_epoch: u64::MAX.into(),
                exit_epoch: u64::MAX.into(),
                withdrawable_epoch: u64::MAX.into(),
            },
        };

        let json = serde_json::to_string(&max_summary).unwrap();
        let deserialized: ValidatorSummary = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.index, u64::MAX, "Should handle u64::MAX for index");
        assert_eq!(deserialized.balance, u64::MAX, "Should handle u64::MAX for balance");
        assert!(deserialized.validator.slashed, "Should preserve slashed status");

        // Test with zero values
        let zero_summary = ValidatorSummary {
            index: 0,
            balance: 0,
            status: ValidatorStatus::PendingInitialized,
            validator: Validator {
                pubkey: BlsPublicKeyBytes::default(),
                withdrawal_credentials: Default::default(),
                effective_balance: 0,
                slashed: false,
                activation_eligibility_epoch: 0u64.into(),
                activation_epoch: 0u64.into(),
                exit_epoch: 0u64.into(),
                withdrawable_epoch: 0u64.into(),
            },
        };

        let json = serde_json::to_string(&zero_summary).unwrap();
        let deserialized: ValidatorSummary = serde_json::from_str(&json).unwrap();
        
        assert_eq!(deserialized.index, 0);
        assert_eq!(deserialized.balance, 0);
    }

    #[test]
    fn test_entry_cloning_preserves_all_fields() {
        let registration_info = create_test_registration_info();
        let pool_name = Some("test_pool_🚀".to_string());
        let user_agent = Some("custom_agent/3.14".to_string());
        
        let entry = SignedValidatorRegistrationEntry::new(
            registration_info,
            pool_name.clone(),
            user_agent.clone(),
        );

        let cloned = entry.clone();

        // All fields should be identical
        assert_eq!(entry.inserted_at, cloned.inserted_at, "Timestamp should be cloned");
        assert_eq!(entry.pool_name, cloned.pool_name, "Pool name should be cloned");
        assert_eq!(entry.user_agent, cloned.user_agent, "User agent should be cloned");
        assert_eq!(
            entry.registration_info.registration.message.gas_limit,
            cloned.registration_info.registration.message.gas_limit,
            "Registration info should be cloned"
        );
    }

    #[test]
    fn test_public_key_returns_reference_not_copy() {
        let registration_info = create_test_registration_info();
        let entry = SignedValidatorRegistrationEntry::new(registration_info, None, None);

        let pubkey1 = entry.public_key();
        let pubkey2 = entry.public_key();

        // Should return the same reference (address), not a copy
        assert_eq!(pubkey1, pubkey2, "Should return consistent reference");
        
        // Verify it's actually pointing to the same data
        assert_eq!(
            pubkey1 as *const BlsPublicKeyBytes,
            pubkey2 as *const BlsPublicKeyBytes,
            "Should return reference to same memory location"
        );
    }
}
