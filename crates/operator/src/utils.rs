use std::collections::HashMap;

use helix_types::{BlsPublicKeyBytes, Demotion, Promotion};
use libp2p::identity::{
    DecodingError, Keypair,
    secp256k1::{self, SecretKey},
};

pub fn load_operator_keypair() -> Keypair {
    let operator_key_str = std::env::var("OPERATOR_KEY").expect("could not load OPERATOR_KEY");
    let mut operator_key_bytes =
        alloy_primitives::hex::decode(operator_key_str).expect("invalid OPERATOR_KEY bytes");
    keypair_from_bytes(&mut operator_key_bytes).expect("failed to decode operator key bytes")
}

pub fn keypair_from_bytes(bytes: &mut [u8]) -> Result<Keypair, DecodingError> {
    let secret_key = SecretKey::try_from_bytes(bytes)?;
    Ok(secp256k1::Keypair::from(secret_key).into())
}

#[derive(Default)]
pub(crate) struct PromotionStates {
    states: HashMap<BlsPublicKeyBytes, PromotionState>,
}

impl PromotionStates {
    pub(crate) fn demoted(&mut self, demotion: Demotion) -> bool {
        let pubkey = demotion.builder_pubkey;
        let state = match self.states.remove(&pubkey) {
            Some(state) => state.try_demote(demotion),
            None => PromotionState::Demoted(demotion),
        };
        let demoted = matches!(state, PromotionState::Demoted(_));
        self.states.insert(pubkey, state);
        demoted
    }

    pub(crate) fn promoted(&mut self, promotion: Promotion) -> bool {
        let pubkey = promotion.builder_pubkey;
        let state = match self.states.remove(&pubkey) {
            Some(state) => state.try_promote(promotion),
            None => PromotionState::Promoted(promotion),
        };
        let promoted = matches!(state, PromotionState::Promoted(_));
        self.states.insert(pubkey, state);
        promoted
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &PromotionState> {
        self.states.values()
    }
}

pub(crate) enum PromotionState {
    Demoted(Demotion),
    Promoted(Promotion),
}

impl PromotionState {
    pub(crate) fn try_demote(self, demotion: Demotion) -> Self {
        match self {
            Self::Demoted(demoted) if demotion.ts_ms > demoted.ts_ms => Self::Demoted(demotion),
            Self::Promoted(promoted) if demotion.ts_ms > promoted.ts_ms => Self::Demoted(demotion),
            other => other,
        }
    }

    pub(crate) fn try_promote(self, promotion: Promotion) -> Self {
        match self {
            Self::Demoted(demoted) if promotion.ts_ms > demoted.ts_ms => Self::Promoted(promotion),
            Self::Promoted(promoted) if promotion.ts_ms > promoted.ts_ms => {
                Self::Promoted(promotion)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use libp2p::identity::Keypair;

    #[test]
    fn keygen() {
        let keypair = Keypair::generate_secp256k1();
        let s_pair = keypair.try_into_secp256k1().unwrap();
        let secret_key_bytes = s_pair.secret().to_bytes();
        let public_key_bytes = s_pair.public().to_bytes();
        println!("private: {}", hex::encode(secret_key_bytes));
        println!("public: {}", hex::encode(public_key_bytes));
    }
}
