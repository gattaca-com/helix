use std::fmt::Debug;
use ethereum_consensus::{
    primitives::{BlsPublicKey, ExecutionAddress, BlsSignature},
    ssz::prelude::*,
};
use crate::builder_api::BuilderGetValidatorsResponseEntry;
use crate::constraints::Constraint;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedPreconferElection {
    pub message: ElectedPreconfer,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedPreconferElection {
    pub fn gateway_info(&self) -> &GatewayInfo {
        &self.message.gateway_info
    }

    pub fn slot(&self) -> u64 {
        self.message.slot
    }

    pub fn proposer_public_key(&self) -> &BlsPublicKey {
        &self.message.proposer_public_key
    }

    pub fn validator_index(&self) -> usize {
        self.message.validator_index
    }
}

impl SignedPreconferElection {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            message: ElectedPreconfer::from_proposer_duty(duty),
            signature: Default::default(),
        }
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ElectedPreconfer {
    /// Slot this delegation is valid for.
    pub slot: u64,
    /// Public key of the validator proposing for `slot`.
    pub proposer_public_key: BlsPublicKey,
    /// Validator index of the validator proposing for `slot`.
    pub validator_index: usize,
    /// Gateway info. Set to default if proposer is handling pre-confirmations.
    /// Note: this should be `None` if the proposer is handling the pre-confirmations.
    /// Haven't quite figured out how to do optional TreeHash for sigp lib, so we just
    /// set this value to a default for proposer pre-confirmations.
    pub gateway_info: GatewayInfo,
}

impl ElectedPreconfer {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry) -> Self {
        Self {
            slot: duty.slot,
            proposer_public_key: duty.entry.registration.message.public_key.clone(),
            validator_index: duty.validator_index,
            gateway_info: GatewayInfo::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GatewayInfo {
    /// Public key of the gateway the proposer is delegating pre-confirmation control to.
    pub gateway_public_key: BlsPublicKey,
    /// Gateway recipient address builder must pay.
    pub gateway_recipient_address: ExecutionAddress,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetGatewayParams {
    /// The slot to get the elected gateway for.
    pub slot: u64,
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedConstraintsMessage {
    pub message: ConstraintsMessage,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedConstraintsMessage {
    pub fn slot(&self) -> u64 {
        self.message.slot
    }

    pub fn constraints(&self) -> &List<Constraint, 4> {
        &self.message.constraints
    }

    pub fn validator_index(&self) -> Option<u64> {
        self.message.validator_index
    }

    pub fn gateway_public_key(&self) -> Option<&BlsPublicKey> {
        self.message.gateway_public_key.as_ref()
    }
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ConstraintsMessage {
    /// Validator index that is setting the constraints.
    /// This will be `Some` if it is the proposer directly.
    pub validator_index: Option<u64>,
    /// Public key of the gateway that is setting the constraints.
    /// This will be `Some` if an elected gateway is setting the constraints.
    pub gateway_public_key: Option<BlsPublicKey>,
    /// Slot these constraints are valid for.
    pub slot: u64,
    pub constraints: List<Constraint, 4>,  // TODO: set const
}

#[cfg(test)]
mod tests {
    use ethereum_consensus::signing::verify_signed_data;
    use reth_primitives::hex;
    use super::*;

    const RHEA_BUILDER_DOMAIN: [u8; 32] = [
        0, 0, 0, 1, 11, 65, 190, 76, 219, 52, 209, 131, 221, 220, 165, 57, 131, 55, 98, 109, 205, 207,
        175, 23, 32, 193, 32, 45, 59, 149, 248, 78,
    ];

    #[test]
    fn test_deserialise_signed_constraints() {
        let hex_str = "7b226d657373616765223a7b2276616c696461746f725f696e646578223a3132332c22676174657761795f7075626c69635f6b6579223a223078303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030222c22736c6f74223a313234352c22636f6e73747261696e7473223a5b7b227478223a2231373034303530363038303930303137303430343035343330383033222c22696e646578223a313233317d5d7d2c227369676e6174757265223a223078303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030303030227d";
        let raw_bytes: Vec<u8> = hex::decode(hex_str).unwrap();
        let res = serde_json::from_slice::<SignedConstraintsMessage>(&raw_bytes);
        assert!(res.is_ok());
    }

    #[test]
    fn test_deserialise_signed_preconfer_election() {
        let hex_str = "7b226d657373616765223a7b22736c6f74223a313239352c2270726f706f7365725f7075626c69635f6b6579223a223078613834393465396134636130623666623539353334376664613034393738656237366330353230623037623736306562636536303963353035316436613765326230316463363866326232656162366163613561386664626264636439333436222c2276616c696461746f725f696e646578223a3131392c22676174657761795f696e666f223a7b22676174657761795f7075626c69635f6b6579223a223078383437303763353433613031396432613434633365663333343637626135346461393637663963623434363861323264346262343465343632336332336265373663393262326562613466323634323838616339363638653037623363656438222c22676174657761795f726563697069656e745f61646472657373223a22307830303030303030303030303030303030303030303030303030303030303030303030303030303030227d7d2c227369676e6174757265223a223078393636303030356534656138616134393863373664363264653539383462663666623665656531353831656330373337393366396337626566333432343264393031383365313836363233373135623833636261643565323133393461353836303662633761306536373033383230613662643232643161323161313638363162383738613365383261346562353730313262383965346239636135323463666363363739303031643231643038656330316432323130363533333064656139227d";
        let raw_bytes: Vec<u8> = hex::decode(hex_str).unwrap();
        let res = serde_json::from_slice::<SignedPreconferElection>(&raw_bytes);
        assert!(res.is_ok())
    }

    #[test]
    fn test_sig_verify_preconfer_election() {
        let hex_str = "7b226d657373616765223a7b22736c6f74223a313239352c2270726f706f7365725f7075626c69635f6b6579223a223078613834393465396134636130623666623539353334376664613034393738656237366330353230623037623736306562636536303963353035316436613765326230316463363866326232656162366163613561386664626264636439333436222c2276616c696461746f725f696e646578223a3131392c22676174657761795f696e666f223a7b22676174657761795f7075626c69635f6b6579223a223078383437303763353433613031396432613434633365663333343637626135346461393637663963623434363861323264346262343465343632336332336265373663393262326562613466323634323838616339363638653037623363656438222c22676174657761795f726563697069656e745f61646472657373223a22307830303030303030303030303030303030303030303030303030303030303030303030303030303030227d7d2c227369676e6174757265223a223078393636303030356534656138616134393863373664363264653539383462663666623665656531353831656330373337393366396337626566333432343264393031383365313836363233373135623833636261643565323133393461353836303662633761306536373033383230613662643232643161323161313638363162383738613365383261346562353730313262383965346239636135323463666363363739303031643231643038656330316432323130363533333064656139227d";
        let mut signed_election = serde_json::from_slice::<SignedPreconferElection>(&hex::decode(hex_str).unwrap()).unwrap();

        let pub_key = signed_election.message.proposer_public_key.clone();
        let res = verify_signed_data(
            &mut signed_election.message,
            &signed_election.signature,
            &pub_key,
            RHEA_BUILDER_DOMAIN,
        );
        println!("{res:?}");
    }
}