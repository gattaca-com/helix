use std::time::Duration;

use helix_common::{
    signing::{RelaySigningContext, RELAY_DOMAIN},
    utils::utcnow_ms,
};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes, EthSpec, MainnetEthSpec,
    SignedRoot, Transaction,
};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use tree_hash_derive::TreeHash;

use crate::socket::WSMessage;

const VALID_DURATION_MS: u64 = Duration::from_secs(5).as_millis() as u64;

#[derive(Debug, thiserror::Error)]
pub enum MessageAuthenticationError {
    #[error("message expiration too far in the future")]
    MessageTooFarInFuture,
    #[error("message already expired")]
    ExpiredMessage,
    #[error("could not deserialize signature")]
    CouldNotDeserializeSignature,
    #[error("signature is invalid")]
    InvalidSignature,
    #[error("could not deserialize public key")]
    CouldNotDeserializePubkey,
}

/// P2P messages, as sent through the wire.
/// [`RawP2PMessage::Other`] is meant to be handled by the main processing logic.
/// The rest of messages are control messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawP2PMessage {
    /// Initial authentication message
    Hello(SignedHelloMessage),
    /// Other, unauthenticated messages
    Other(P2PMessage),
}

impl RawP2PMessage {
    #[expect(clippy::result_large_err)]
    pub fn to_ws_message(&self) -> Result<WSMessage, EncodingError> {
        let text = serde_json::to_string(&self)?;
        Ok(WSMessage::text(text))
    }

    #[expect(clippy::result_large_err)]
    pub fn from_ws_message(message: &WSMessage) -> Result<Self, EncodingError> {
        let text = message.to_text()?;
        Ok(serde_json::from_str(text)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum P2PMessage {
    InclusionList(InclusionListMessage),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EncodingError {
    #[error("serde error: {_0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid utf8: {_0}")]
    InvalidUtf8(#[from] tokio_tungstenite::tungstenite::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignedHelloMessage {
    pub(crate) message: HelloMessage,
    /// Signature over SSZ hash-tree-root of message.
    signature: BlsSignatureBytes,
}

impl SignedHelloMessage {
    pub(crate) fn new(signing_context: &RelaySigningContext) -> Self {
        let message = HelloMessage::new(signing_context.pubkey);
        let signature = signing_context.sign_relay_message(&message).serialize().into();
        Self { message, signature }
    }

    pub(crate) fn pubkey(&self) -> Result<BlsPublicKey, MessageAuthenticationError> {
        BlsPublicKey::deserialize(&*self.message.pubkey)
            .map_err(|_| MessageAuthenticationError::CouldNotDeserializePubkey)
    }

    pub(crate) fn verify_signature(
        &self,
        pubkey: &BlsPublicKey,
    ) -> Result<(), MessageAuthenticationError> {
        let now = utcnow_ms();
        if self.message.valid_until < now {
            return Err(MessageAuthenticationError::ExpiredMessage);
        } else if self.message.valid_until > now + 2 * VALID_DURATION_MS {
            return Err(MessageAuthenticationError::MessageTooFarInFuture);
        }
        let signing_root = self.message.signing_root(RELAY_DOMAIN.into());

        let signature = BlsSignature::deserialize(self.signature.as_ref())
            .map_err(|_| MessageAuthenticationError::CouldNotDeserializeSignature)?;

        if !signature.verify(pubkey, signing_root) {
            return Err(MessageAuthenticationError::InvalidSignature);
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
