use helix_common::signing::{RelaySigningContext, RELAY_DOMAIN};
use helix_types::{BlsPublicKey, BlsSignature, EthSpec, MainnetEthSpec, SignedRoot, Transaction};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use tree_hash_derive::TreeHash;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedP2PMessage {
    pub message: P2PMessage,
    /// Signature over SSZ hash-tree-root of message.
    signature: BlsSignature,
}

#[derive(Debug, thiserror::Error)]
#[error("message verification failed")]
pub struct MessageVerificationError;

impl SignedP2PMessage {
    pub fn verify_signature(&self, pubkey: &BlsPublicKey) -> Result<(), MessageVerificationError> {
        let signing_root = self.message.signing_root(RELAY_DOMAIN.into());
        if self.signature.verify(pubkey, signing_root) {
            Ok(())
        } else {
            Err(MessageVerificationError)
        }
    }
}

// These impls are to avoid code duplication between both websockets
impl TryFrom<axum::extract::ws::Message> for SignedP2PMessage {
    type Error = String;

    fn try_from(value: axum::extract::ws::Message) -> Result<Self, Self::Error> {
        let text = value.into_text().map_err(|e| format!("Failed to convert to text: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("Failed to deserialize SignedP2PMessage: {e}"))
    }
}

impl TryFrom<tokio_tungstenite::tungstenite::Message> for SignedP2PMessage {
    type Error = String;

    fn try_from(value: tokio_tungstenite::tungstenite::Message) -> Result<Self, Self::Error> {
        let text = value.into_text().map_err(|e| format!("Failed to convert to text: {e}"))?;
        serde_json::from_str(&text)
            .map_err(|e| format!("Failed to deserialize SignedP2PMessage: {e}"))
    }
}

impl TryFrom<SignedP2PMessage> for axum::extract::ws::Message {
    type Error = String;

    fn try_from(value: SignedP2PMessage) -> Result<Self, Self::Error> {
        let text = serde_json::to_string(&value)
            .map_err(|e| format!("Failed to serialize SignedP2PMessage: {e}"))?;
        Ok(axum::extract::ws::Message::text(text))
    }
}

impl TryFrom<SignedP2PMessage> for tokio_tungstenite::tungstenite::Message {
    type Error = String;

    fn try_from(value: SignedP2PMessage) -> Result<Self, Self::Error> {
        let text = serde_json::to_string(&value)
            .map_err(|e| format!("Failed to serialize SignedP2PMessage: {e}"))?;
        Ok(tokio_tungstenite::tungstenite::Message::text(text))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
#[ssz(enum_behaviour = "union")]
#[tree_hash(enum_behaviour = "transparent")]
pub enum P2PMessage {
    LocalInclusionList(InclusionListMessage),
}

impl helix_types::SignedRoot for P2PMessage {}

impl P2PMessage {
    pub(crate) fn sign(self, signing_context: &RelaySigningContext) -> SignedP2PMessage {
        let signature = signing_context.sign_relay_message(&self);
        SignedP2PMessage { message: self, signature }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct InclusionListMessage {
    pub slot: u64,
    pub inclusion_list: InclusionList,
}

impl From<InclusionListMessage> for P2PMessage {
    fn from(msg: InclusionListMessage) -> Self {
        P2PMessage::LocalInclusionList(msg)
    }
}

pub type InclusionList =
    VariableList<Transaction, <MainnetEthSpec as EthSpec>::MaxTransactionsPerPayload>;
