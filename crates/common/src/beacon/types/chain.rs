use alloy_primitives::{Address, B256, hex};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes, Domain, EthSpec,
    MainnetEthSpec, SignedRoot, Slot, Withdrawals,
};
use serde::{Deserialize, Serialize};
use tree_hash_derive::TreeHash;

use crate::chain_info::ChainInfo;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum StateId {
    Head,
    Genesis,
    Finalized,
    Justified,
    Slot(Slot),
    Root(B256),
}

impl std::fmt::Display for StateId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let printable = match *self {
            StateId::Finalized => "finalized",
            StateId::Justified => "justified",
            StateId::Head => "head",
            StateId::Genesis => "genesis",
            StateId::Slot(slot) => return write!(f, "{slot}"),
            StateId::Root(root) => return write!(f, "{root}"),
        };
        write!(f, "{printable}")
    }
}

impl std::str::FromStr for StateId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "finalized" => Ok(StateId::Finalized),
            "justified" => Ok(StateId::Justified),
            "head" => Ok(StateId::Head),
            "genesis" => Ok(StateId::Genesis),
            _ => match s.parse::<Slot>() {
                Ok(slot) => Ok(Self::Slot(slot)),
                Err(_) => match hex::decode(s) {
                    Ok(root_data) => {
                        let root = B256::try_from(root_data.as_slice()).map_err(|err| {
                            format!(
                                "could not parse state identifier by root from the provided argument {s}: {err}"
                            )
                        })?;
                        Ok(Self::Root(root))
                    }
                    Err(err) => Err(format!(
                        "could not parse state identifier by root from the provided argument {s}: {err}"
                    )),
                },
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SyncStatus {
    pub head_slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub sync_distance: u64,
    pub is_syncing: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct HeadEventData {
    pub slot: Slot,
    pub block: B256,
    pub state: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProposerPreferencesEvent {
    pub version: String,
    pub data: SignedProposerPreferences,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SignedProposerPreferences {
    pub message: ProposerPreferences,
    pub signature: BlsSignatureBytes,
}

impl SignedRoot for ProposerPreferences {}

impl SignedProposerPreferences {
    /// Whether `pubkey` signed these preferences. The fee recipient a builder pays
    /// comes from here, so an unsigned message would let anyone redirect it.
    pub fn verify(&self, pubkey: &BlsPublicKeyBytes, chain_info: &ChainInfo) -> bool {
        let epoch = self.message.proposal_slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::ProposerPreferences,
            &fork,
            chain_info.genesis_validators_root,
        );
        let Ok(pubkey) = BlsPublicKey::deserialize(pubkey.as_ref()) else {
            return false;
        };
        let Ok(signature) = BlsSignature::deserialize(self.signature.as_ref()) else {
            return false;
        };
        signature.verify(&pubkey, self.message.signing_root(domain))
    }
}

/// Gossiped once per proposal slot, per
/// <https://github.com/ethereum/consensus-specs/blob/master/specs/gloas/p2p-interface.md#new-signedproposerpreferences>.
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, TreeHash)]
pub struct ProposerPreferences {
    pub dependent_root: B256,
    pub proposal_slot: Slot,
    #[serde(with = "serde_utils::quoted_u64")]
    pub validator_index: u64,
    pub fee_recipient: Address,
    #[serde(with = "serde_utils::quoted_u64")]
    pub target_gas_limit: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributesEvent {
    pub version: String,
    pub data: PayloadAttributesEventData,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributesEventData {
    #[serde(with = "serde_utils::quoted_u64")]
    pub proposer_index: u64,
    pub proposal_slot: Slot,
    /// Absent from Gloas, which no longer carries it.
    #[serde(default, with = "quoted_u64_opt")]
    pub parent_block_number: Option<u64>,
    pub parent_block_root: String,
    pub parent_block_hash: B256,
    pub payload_attributes: PayloadAttributes,
}

mod quoted_u64_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_utils::quoted_u64::Quoted;

    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        value.map(|value| Quoted { value }).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Ok(Option::<Quoted<u64>>::deserialize(deserializer)?.map(|quoted| quoted.value))
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PayloadAttributes {
    #[serde(with = "serde_utils::quoted_u64")]
    pub timestamp: u64,
    pub prev_randao: B256,
    pub suggested_fee_recipient: String,
    pub withdrawals: Withdrawals,
    pub parent_beacon_block_root: Option<B256>,
}

#[cfg(test)]
mod proposer_preferences_signature_tests {
    use helix_types::BlsKeypair;

    use super::*;

    fn prefs(slot: u64) -> ProposerPreferences {
        ProposerPreferences {
            dependent_root: B256::repeat_byte(0x11),
            proposal_slot: Slot::new(slot),
            validator_index: 42,
            fee_recipient: Address::repeat_byte(0x22),
            target_gas_limit: 45_000_000,
        }
    }

    fn sign(
        message: &ProposerPreferences,
        keypair: &BlsKeypair,
        chain_info: &ChainInfo,
    ) -> BlsSignatureBytes {
        let epoch = message.proposal_slot.epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        let domain = chain_info.spec.get_domain(
            epoch,
            Domain::ProposerPreferences,
            &fork,
            chain_info.genesis_validators_root,
        );
        keypair.sk.sign(message.signing_root(domain)).serialize().into()
    }

    #[test]
    fn the_proposers_own_signature_verifies() {
        crate::utils::install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let keypair = BlsKeypair::random();
        let message = prefs(100);
        let signed =
            SignedProposerPreferences { signature: sign(&message, &keypair, &chain_info), message };

        assert!(signed.verify(&keypair.pk.serialize().into(), &chain_info));
    }

    /// The whole point: preferences signed by someone else must not be accepted for
    /// this proposer, or the fee recipient the builder pays can be redirected.
    #[test]
    fn another_keys_signature_is_refused() {
        crate::utils::install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let proposer = BlsKeypair::random();
        let attacker = BlsKeypair::random();
        let message = prefs(100);
        let signed = SignedProposerPreferences {
            signature: sign(&message, &attacker, &chain_info),
            message,
        };

        assert!(!signed.verify(&proposer.pk.serialize().into(), &chain_info));
    }

    #[test]
    fn a_tampered_fee_recipient_is_refused() {
        crate::utils::install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let keypair = BlsKeypair::random();
        let message = prefs(100);
        let signature = sign(&message, &keypair, &chain_info);
        let mut tampered = message;
        tampered.fee_recipient = Address::repeat_byte(0xff);
        let signed = SignedProposerPreferences { message: tampered, signature };

        assert!(!signed.verify(&keypair.pk.serialize().into(), &chain_info));
    }
}
