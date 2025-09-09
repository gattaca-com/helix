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

use crate::socket::WSMessage;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum P2PMessage {
    Hello(SignedHelloMessage),
    InclusionList(InclusionListMessage),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EncodingError {
    #[error("serde error: {_0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid utf8: {_0}")]
    InvalidUtf8(#[from] tokio_tungstenite::tungstenite::Error),
}

impl P2PMessage {
    pub fn to_ws_message(&self) -> Result<WSMessage, EncodingError> {
        let text = serde_json::to_string(&self)?;
        Ok(WSMessage::text(text))
    }

    pub fn from_ws_message(message: &WSMessage) -> Result<Self, EncodingError> {
        let text = message.to_text()?;
        Ok(serde_json::from_str(&text)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SignedHelloMessage {
    pub(crate) message: HelloMessage,
    /// Signature over SSZ hash-tree-root of message.
    // TODO: serialize
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
