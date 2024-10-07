use std::fmt::Debug;

use ethereum_consensus::{
    deneb::{minimal::MAX_BYTES_PER_TRANSACTION, Transaction},
    primitives::{BlsPublicKey, BlsSignature},
    ssz::prelude::*,
};

use crate::builder_api::BuilderGetValidatorsResponseEntry;

pub const MAX_TRANSACTIONS_PER_BLOCK: usize = 10_000;

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct SignedPreconferElection {
    pub message: PreconferElection,
    /// Signature over `message`. Must be signed by the key relating to: `message.public_key`.
    pub signature: BlsSignature,
}

impl SignedPreconferElection {
    pub fn slot(&self) -> u64 {
        self.message.slot()
    }

    pub fn preconfer_public_key(&self) -> &BlsPublicKey {
        &self.message.preconfer_public_key()
    }

    pub fn chain_id(&self) -> u64 {
        self.message.chain_id()
    }

    pub fn gas_limit(&self) -> u64 {
        self.message.gas_limit()
    }
}

impl SignedPreconferElection {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry, chain_id: u64) -> Self {
        Self { message: PreconferElection::from_proposer_duty(duty, chain_id), signature: Default::default() }
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct PreconferElection {
    /// Public key of the preconfer proposing for `slot`.
    preconfer_pubkey: BlsPublicKey,
    /// Slot this delegation is valid for.
    slot_number: u64,
    /// Chain ID of the chain this election is for.
    chain_id: u64,
    // The gas limit specified by the proposer that the preconfer must adhere to.
    gas_limit: u64,
}

impl PreconferElection {
    pub fn from_proposer_duty(duty: &BuilderGetValidatorsResponseEntry, chain_id: u64) -> Self {
        Self {
            slot_number: duty.slot,
            preconfer_pubkey: duty.entry.registration.message.public_key.clone(),
            chain_id,
            gas_limit: duty.entry.registration.message.gas_limit,
        }
    }

    pub fn slot(&self) -> u64 {
        self.slot_number
    }

    pub fn preconfer_public_key(&self) -> &BlsPublicKey {
        &self.preconfer_pubkey
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetConstraintsParams {
    // The slot to get the constraints for.
    pub slot: u64,
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct GetPreconferParams {
    // The slot to get the preconfer election for.
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
        self.message.slot()
    }

    pub fn constraints(&self) -> &List<List<Constraint, MAX_TRANSACTIONS_PER_BLOCK>, MAX_TRANSACTIONS_PER_BLOCK> {
        self.message.constraints()
    }
}

#[derive(Debug, Default, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct ConstraintsMessage {
    slot: u64,
    constraints: List<List<Constraint, MAX_TRANSACTIONS_PER_BLOCK>, MAX_TRANSACTIONS_PER_BLOCK>,
}

impl ConstraintsMessage {
    pub fn new(slot: u64, constraints: List<List<Constraint, MAX_TRANSACTIONS_PER_BLOCK>, MAX_TRANSACTIONS_PER_BLOCK>) -> Self {
        Self { slot, constraints }
    }

    pub fn slot(&self) -> u64 {
        self.slot
    }

    pub fn constraints(&self) -> &List<List<Constraint, MAX_TRANSACTIONS_PER_BLOCK>, MAX_TRANSACTIONS_PER_BLOCK> {
        &self.constraints
    }

    pub fn add_constraints(&mut self, constraints: List<Constraint, MAX_TRANSACTIONS_PER_BLOCK>) {
        self.constraints.push(constraints);
    }
}

#[derive(Debug, Clone, SimpleSerialize, serde::Serialize, serde::Deserialize, Default)]
pub struct Constraint {
    tx: Transaction<MAX_BYTES_PER_TRANSACTION>,
}

impl Constraint {
    pub fn tx(&self) -> &Transaction<MAX_BYTES_PER_TRANSACTION> {
        &self.tx
    }
}

#[cfg(test)]
mod tests {
    use ethereum_consensus::signing::verify_signed_data;
    use reth_primitives::hex;

    use super::*;

    const RHEA_BUILDER_DOMAIN: [u8; 32] =
        [0, 0, 0, 1, 11, 65, 190, 76, 219, 52, 209, 131, 221, 220, 165, 57, 131, 55, 98, 109, 205, 207, 175, 23, 32, 193, 32, 45, 59, 149, 248, 78];

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

        let pub_key = signed_election.message.preconfer_pubkey.clone();
        let res = verify_signed_data(&mut signed_election.message, &signed_election.signature, &pub_key, RHEA_BUILDER_DOMAIN);
        assert!(res.is_ok());
    }

    #[test]
    fn test_sig_verify_constraints() {
        let hex_str = "7b226d657373616765223a7b2276616c696461746f725f696e646578223a3132332c22676174657761795f7075626c69635f6b6579223a223078613834393465396134636130623666623539353334376664613034393738656237366330353230623037623736306562636536303963353035316436613765326230316463363866326232656162366163613561386664626264636439333436222c22736c6f74223a313234352c22636f6e73747261696e7473223a5b7b227478223a2231373034303530363038303930303137303430343035343330383033222c22696e646578223a313233317d5d7d2c227369676e6174757265223a223078613731343765633766613537313635323462633239366361346665316534376266623462306665646666666332636236623661373465353236656364653164636462356230616265633139346266326161616363626130313937643930383535313064383336633936653932616537613465633062376239346531666634333838396262306637643037656261303938363464316363356663316166336662303231396134313631623337616137666331626534653035623737346263343866227d";
        let mut signed_constraints = serde_json::from_slice::<SignedConstraintsMessage>(&hex::decode(hex_str).unwrap()).unwrap();

        let pub_key = Default::default();
        let res = verify_signed_data(&mut signed_constraints.message, &signed_constraints.signature, &pub_key, RHEA_BUILDER_DOMAIN);
        assert!(res.is_ok());
    }

    #[test]
    fn test_serialise() {
        let constraints_array = vec![Constraint { tx: Default::default() }, Constraint { tx: Default::default() }];
        let constraints_list_a = List::try_from(constraints_array.clone()).unwrap();
        let constraints_list_b = List::try_from(constraints_array).unwrap();
        let list_of_lists = vec![constraints_list_a, constraints_list_b];

        let constraint_message = ConstraintsMessage { slot: 88, constraints: List::try_from(list_of_lists).unwrap() };

        let json_serialised = serde_json::to_string_pretty(&constraint_message).unwrap();
        println!("{}", json_serialised);

        let json_to_vec = serde_json::to_vec(&constraint_message).unwrap();
        println!("{:?}", json_to_vec);
    }
}
