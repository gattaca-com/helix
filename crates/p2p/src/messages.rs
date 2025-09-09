use std::time::Duration;

use helix_common::{
    signing::{RelaySigningContext, RELAY_DOMAIN},
    utils::utcnow_ms,
};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, BlsSignature, EthSpec, MainnetEthSpec, SignedRoot, Transaction,
};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use tree_hash_derive::TreeHash;

const VALID_DURATION_MS: u64 = Duration::from_secs(5).as_millis() as u64;

#[derive(Debug, thiserror::Error)]
pub enum MessageVerificationError {
    #[error("message expiration too far in the future")]
    MessageTooFarInFuture,
    #[error("message already expired")]
    ExpiredMessage,
    #[error("signature is invalid")]
    InvalidSignature,
    #[error("could not deserialize public key")]
    CouldNotDeserializePubkey,
}

// These impls are to avoid code duplication between both websockets
impl TryFrom<axum::extract::ws::Message> for P2PMessage {
    type Error = String;

    fn try_from(value: axum::extract::ws::Message) -> Result<Self, Self::Error> {
        let text = value.into_text().map_err(|e| format!("Failed to convert to text: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("Failed to deserialize P2PMessage: {e}"))
    }
}

impl TryFrom<tokio_tungstenite::tungstenite::Message> for P2PMessage {
    type Error = String;

    fn try_from(value: tokio_tungstenite::tungstenite::Message) -> Result<Self, Self::Error> {
        let text = value.into_text().map_err(|e| format!("Failed to convert to text: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("Failed to deserialize P2PMessage: {e}"))
    }
}

impl TryFrom<P2PMessage> for axum::extract::ws::Message {
    type Error = String;

    fn try_from(value: P2PMessage) -> Result<Self, Self::Error> {
        let text = serde_json::to_string(&value)
            .map_err(|e| format!("Failed to serialize P2PMessage: {e}"))?;
        Ok(axum::extract::ws::Message::text(text))
    }
}

impl TryFrom<P2PMessage> for tokio_tungstenite::tungstenite::Message {
    type Error = String;

    fn try_from(value: P2PMessage) -> Result<Self, Self::Error> {
        let text = serde_json::to_string(&value)
            .map_err(|e| format!("Failed to serialize P2PMessage: {e}"))?;
        Ok(tokio_tungstenite::tungstenite::Message::text(text))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum P2PMessage {
    Hello(SignedHelloMessage),
    InclusionList(InclusionListMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignedHelloMessage {
    pub(crate) message: HelloMessage,
    /// Signature over SSZ hash-tree-root of message.
    signature: BlsSignature,
}

impl SignedHelloMessage {
    pub(crate) fn new(signing_context: &RelaySigningContext) -> Self {
        let message = HelloMessage::new(signing_context.pubkey.clone());
        let signature = signing_context.sign_relay_message(&message);
        Self { message, signature }
    }

    pub(crate) fn pubkey(&self) -> Result<BlsPublicKey, MessageVerificationError> {
        BlsPublicKey::deserialize(&*self.message.pubkey)
            .map_err(|_| MessageVerificationError::CouldNotDeserializePubkey)
    }

    pub(crate) fn verify_signature(
        &self,
        pubkey: &BlsPublicKey,
    ) -> Result<(), MessageVerificationError> {
        let now = utcnow_ms();
        if self.message.valid_until < now {
            return Err(MessageVerificationError::ExpiredMessage);
        } else if self.message.valid_until > now + 2 * VALID_DURATION_MS {
            return Err(MessageVerificationError::MessageTooFarInFuture);
        }
        let signing_root = self.message.signing_root(RELAY_DOMAIN.into());

        if !self.signature.verify(pubkey, signing_root) {
            return Err(MessageVerificationError::InvalidSignature);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct HelloMessage {
    /// BLS public key of the sender.
    pub pubkey: BlsPublicKeyBytes,
    /// Timestamp to avoid replay attacks.
    valid_until: u64,
}

impl helix_types::SignedRoot for HelloMessage {}

impl HelloMessage {
    fn new(pubkey: BlsPublicKeyBytes) -> Self {
        let valid_until = utcnow_ms() + VALID_DURATION_MS;
        Self { pubkey, valid_until }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
pub struct InclusionListMessage {
    pub slot: u64,
    pub inclusion_list: InclusionList,
}

impl InclusionListMessage {
    pub fn new(slot: u64, inclusion_list: helix_common::api::builder_api::InclusionList) -> Self {
        let inclusion_list =
            inclusion_list.txs.into_iter().map(Transaction).collect::<Vec<_>>().into();

        Self { slot, inclusion_list }
    }
}

impl From<InclusionListMessage> for P2PMessage {
    fn from(msg: InclusionListMessage) -> Self {
        P2PMessage::InclusionList(msg)
    }
}

pub type InclusionList =
    VariableList<Transaction, <MainnetEthSpec as EthSpec>::MaxTransactionsPerPayload>;
