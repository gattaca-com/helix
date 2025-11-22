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
}
