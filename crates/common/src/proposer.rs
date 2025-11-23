use helix_types::{BlsPublicKeyBytes, SignedValidatorRegistration, Slot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerDuty {
    pub pubkey: BlsPublicKeyBytes,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub slot: Slot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposerSchedule {
    pub slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub entry: SignedValidatorRegistration,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProposerInfo {
    pub name: String,
    pub pubkey: BlsPublicKeyBytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proposer_duty_serialization() {
        let duty = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 12345,
            slot: Slot::new(100),
        };

        let serialized = serde_json::to_string(&duty).unwrap();
        let deserialized: ProposerDuty = serde_json::from_str(&serialized).unwrap();

        assert_eq!(duty.validator_index, deserialized.validator_index);
        assert_eq!(duty.slot, deserialized.slot);
    }

    #[test]
    fn test_proposer_duty_clone() {
        let duty1 = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 42,
            slot: Slot::new(50),
        };

        let duty2 = duty1.clone();
        assert_eq!(duty1.validator_index, duty2.validator_index);
        assert_eq!(duty1.slot, duty2.slot);
    }

    #[test]
    fn test_proposer_schedule_serialization() {
        let schedule = ProposerSchedule {
            slot: Slot::new(200),
            validator_index: 9999,
            entry: SignedValidatorRegistration {
                message: helix_types::ValidatorRegistrationData {
                    fee_recipient: alloy_primitives::Address::ZERO,
                    gas_limit: 30_000_000,
                    timestamp: 1234567890,
                    pubkey: BlsPublicKeyBytes::default(),
                },
                signature: Default::default(),
            },
        };

        let serialized = serde_json::to_string(&schedule).unwrap();
        let deserialized: ProposerSchedule = serde_json::from_str(&serialized).unwrap();

        assert_eq!(schedule.slot, deserialized.slot);
        assert_eq!(schedule.validator_index, deserialized.validator_index);
    }

    #[test]
    fn test_proposer_info_equality() {
        let info1 = ProposerInfo {
            name: "Test Proposer".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };

        let info2 = ProposerInfo {
            name: "Test Proposer".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };

        let info3 = ProposerInfo {
            name: "Different Proposer".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };

        assert_eq!(info1, info2);
        assert_ne!(info1, info3);
    }

    #[test]
    fn test_proposer_info_clone() {
        let info1 = ProposerInfo {
            name: "Validator 1".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };

        let info2 = info1.clone();
        assert_eq!(info1, info2);
    }

    #[test]
    fn test_proposer_info_serialization() {
        let info = ProposerInfo {
            name: "Test".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };

        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: ProposerInfo = serde_json::from_str(&serialized).unwrap();

        assert_eq!(info, deserialized);
    }

    #[test]
    fn test_proposer_duty_debug() {
        let duty = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 123,
            slot: Slot::new(456),
        };

        let debug_str = format!("{:?}", duty);
        assert!(debug_str.contains("ProposerDuty"));
        assert!(debug_str.contains("123"));
    }

    #[test]
    fn test_proposer_duty_edge_cases() {
        // Slot 0 (genesis)
        let genesis_duty = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 0,
            slot: Slot::new(0),
        };
        let json = serde_json::to_string(&genesis_duty).unwrap();
        assert!(json.contains("\"validator_index\":\"0\""), "Should quote u64");
        let deserialized: ProposerDuty = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.slot, Slot::new(0));

        // Maximum validator index
        let max_validator = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: u64::MAX,
            slot: Slot::new(1000),
        };
        let json = serde_json::to_string(&max_validator).unwrap();
        let deserialized: ProposerDuty = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.validator_index, u64::MAX);

        // Very large slot number
        let large_slot = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 12345,
            slot: Slot::new(10_000_000),
        };
        let json = serde_json::to_string(&large_slot).unwrap();
        let deserialized: ProposerDuty = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.slot.as_u64(), 10_000_000);
    }

    #[test]
    fn test_proposer_info_name_edge_cases() {
        // Empty name
        let empty = ProposerInfo {
            name: String::new(),
            pubkey: BlsPublicKeyBytes::default(),
        };
        let json = serde_json::to_string(&empty).unwrap();
        let deserialized: ProposerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "");

        // Very long name
        let long_name = "a".repeat(10_000);
        let long = ProposerInfo {
            name: long_name.clone(),
            pubkey: BlsPublicKeyBytes::default(),
        };
        let json = serde_json::to_string(&long).unwrap();
        let deserialized: ProposerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name.len(), 10_000);

        // Unicode name
        let unicode = ProposerInfo {
            name: "Validator 验证者 🚀".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };
        let json = serde_json::to_string(&unicode).unwrap();
        let deserialized: ProposerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "Validator 验证者 🚀");

        // Special characters
        let special = ProposerInfo {
            name: "Test\nName\tWith\rSpecial\"Chars".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };
        let json = serde_json::to_string(&special).unwrap();
        let deserialized: ProposerInfo = serde_json::from_str(&json).unwrap();
        assert!(deserialized.name.contains("\n"));
        assert!(deserialized.name.contains("\t"));
    }

    #[test]
    fn test_proposer_info_equality_with_different_pubkeys() {
        let pubkey1 = BlsPublicKeyBytes::default();
        let mut pubkey2_bytes = [0u8; 48];
        pubkey2_bytes[0] = 1; // Different pubkey
        let pubkey2 = BlsPublicKeyBytes::from(pubkey2_bytes);

        let info1 = ProposerInfo {
            name: "Same Name".to_string(),
            pubkey: pubkey1,
        };

        let info2 = ProposerInfo {
            name: "Same Name".to_string(),
            pubkey: pubkey2,
        };

        // Same name but different pubkey = not equal
        assert_ne!(info1, info2, "Should be different with different pubkeys");
    }

    #[test]
    fn test_proposer_schedule_slot_validation() {
        // ProposerSchedule associates a slot with a validator
        let schedule = ProposerSchedule {
            slot: Slot::new(32), // First slot of epoch 1
            validator_index: 100,
            entry: SignedValidatorRegistration {
                message: helix_types::ValidatorRegistrationData {
                    fee_recipient: alloy_primitives::Address::ZERO,
                    gas_limit: 30_000_000,
                    timestamp: 1234567890,
                    pubkey: BlsPublicKeyBytes::default(),
                },
                signature: Default::default(),
            },
        };

        // Verify slot and validator_index are serialized as expected
        let json = serde_json::to_string(&schedule).unwrap();
        assert!(json.contains("\"validator_index\":\"100\""));
        
        let deserialized: ProposerSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.slot, Slot::new(32));
        assert_eq!(deserialized.validator_index, 100);
    }

    #[test]
    fn test_all_structs_implement_debug() {
        // Ensure Debug is properly implemented for all types
        let duty = ProposerDuty {
            pubkey: BlsPublicKeyBytes::default(),
            validator_index: 1,
            slot: Slot::new(1),
        };
        assert!(format!("{:?}", duty).contains("ProposerDuty"));

        let schedule = ProposerSchedule {
            slot: Slot::new(1),
            validator_index: 1,
            entry: SignedValidatorRegistration {
                message: helix_types::ValidatorRegistrationData {
                    fee_recipient: alloy_primitives::Address::ZERO,
                    gas_limit: 30_000_000,
                    timestamp: 1,
                    pubkey: BlsPublicKeyBytes::default(),
                },
                signature: Default::default(),
            },
        };
        assert!(format!("{:?}", schedule).contains("ProposerSchedule"));

        let info = ProposerInfo {
            name: "Test".to_string(),
            pubkey: BlsPublicKeyBytes::default(),
        };
        assert!(format!("{:?}", info).contains("ProposerInfo"));
    }
}
