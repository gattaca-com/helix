use std::time::Duration;

use helix_common::{
    api::builder_api::InclusionList,
    signing::{RelaySigningContext, RELAY_DOMAIN},
    utils::utcnow_ms,
};
use helix_types::{BlsPublicKey, BlsPublicKeyBytes, BlsSignature, BlsSignatureBytes, SignedRoot};
use serde::{Deserialize, Serialize};
use ssz_derive::{Decode, Encode};
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

/// Messages, as sent through the wire.
/// [`RawNetworkMessage::Other`] variant is meant to be handled by
/// [crate::RelayNetworkApi::handle_requests]. The rest of messages are control messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawNetworkMessage {
    /// Initial authentication message
    Hello(HelloMessage),
    /// Other, unauthenticated messages
    Other(NetworkMessage),
}

impl RawNetworkMessage {
    pub fn to_ws_message(&self) -> WSMessage {
        let text = serde_json::to_string(&self).expect("encoding cannot fail");
        WSMessage::text(text)
    }

    pub fn from_ws_message(message: &WSMessage) -> Result<Self, EncodingError> {
        let text = message.to_text().map_err(Box::new)?;
        Ok(serde_json::from_str(text)?)
    }
}

/// Messages, as seen by the main processing logic in [crate::RelayNetworkApi::handle_requests].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum NetworkMessage {
    LocalInclusionList(InclusionListMessage),
    SharedInclusionList(InclusionListMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum NetworkMessageType {
    /// Inclusion list related messages.
    /// - [`NetworkMessage::LocalInclusionList`]
    /// - [`NetworkMessage::SharedInclusionList`]
    InclusionList,
    #[serde(other)]
    Unknown,
}

impl NetworkMessageType {
    pub(crate) fn supports_message(&self, message: &NetworkMessage) -> bool {
        match (self, message) {
            (NetworkMessageType::InclusionList, NetworkMessage::LocalInclusionList(_)) => true,
            (NetworkMessageType::InclusionList, NetworkMessage::SharedInclusionList(_)) => true,
            (NetworkMessageType::Unknown, _) => false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EncodingError {
    #[error("serde error: {_0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid utf8: {_0}")]
    // Boxed due to big size
    InvalidUtf8(#[from] Box<tokio_tungstenite::tungstenite::Error>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HelloMessage {
    /// BLS public key of the sender.
    pub pubkey: BlsPublicKeyBytes,
    /// Timestamp to avoid replay attacks.
    valid_until: u64,
    /// Signature over SSZ hash-tree-root of [`Self::pubkey`] and [`Self::valid_until`].
    /// See [`MessageToSign`] for the specific format.
    signature: BlsSignatureBytes,
    /// List of message types the sender supports.
    /// It's expected the receiver won't send messages of types not in this list.
    #[serde(default)]
    supported_message_types: Vec<NetworkMessageType>,
}

/// Used for generating the [`signature`](HelloMessage::signature) in [`HelloMessage`].
#[derive(Debug, Clone, Serialize, Deserialize, Encode, Decode, TreeHash)]
struct MessageToSign {
    pubkey: BlsPublicKeyBytes,
    valid_until: u64,
}

impl helix_types::SignedRoot for MessageToSign {}

impl HelloMessage {
    pub(crate) fn new(signing_context: &RelaySigningContext) -> Self {
        let valid_until = utcnow_ms() + VALID_DURATION_MS;
        let pubkey = signing_context.pubkey;
        let message = MessageToSign { pubkey, valid_until };
        let signature = signing_context.sign_relay_message(&message).serialize().into();
        Self { pubkey, valid_until, signature, supported_message_types: vec![] }
    }

    pub(crate) fn with_supported_message_types(
        mut self,
        supported_message_types: Vec<NetworkMessageType>,
    ) -> Self {
        self.supported_message_types = supported_message_types;
        self
    }

    pub(crate) fn into_supported_message_types(self) -> Vec<NetworkMessageType> {
        let mut smt = self.supported_message_types;
        // Remove duplicates
        smt.sort();
        smt.dedup();
        // Drop unknown message types
        smt.retain(|m| !matches!(m, NetworkMessageType::Unknown));
        smt
    }

    pub(crate) fn deserialize_pubkey(&self) -> Result<BlsPublicKey, MessageAuthenticationError> {
        BlsPublicKey::deserialize(&*self.pubkey)
            .map_err(|_| MessageAuthenticationError::CouldNotDeserializePubkey)
    }

    pub(crate) fn verify_signature(
        &self,
        pubkey: &BlsPublicKey,
    ) -> Result<(), MessageAuthenticationError> {
        let now = utcnow_ms();
        if self.valid_until < now {
            return Err(MessageAuthenticationError::ExpiredMessage);
        } else if self.valid_until > now + 2 * VALID_DURATION_MS {
            return Err(MessageAuthenticationError::MessageTooFarInFuture);
        }
        let message = MessageToSign { pubkey: self.pubkey, valid_until: self.valid_until };
        let signing_root = message.signing_root(RELAY_DOMAIN.into());

        let signature = BlsSignature::deserialize(self.signature.as_ref())
            .map_err(|_| MessageAuthenticationError::CouldNotDeserializeSignature)?;

        if !signature.verify(pubkey, signing_root) {
            return Err(MessageAuthenticationError::InvalidSignature);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InclusionListMessage {
    pub slot: u64,
    pub inclusion_list: InclusionList,
}

impl InclusionListMessage {
    pub fn new(slot: u64, inclusion_list: InclusionList) -> Self {
        Self { slot, inclusion_list }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use alloy_primitives::Bytes;
    use helix_common::api::builder_api::InclusionList;
    use helix_types::Transaction;
    use serde::{Deserialize, Serialize};

    use crate::messages::{HelloMessage, InclusionListMessage, NetworkMessage, RawNetworkMessage};

    fn test_roundtrip<T: Serialize + for<'de> Deserialize<'de> + Debug>(msg: &T) {
        let serialized = serde_json::to_string(msg).unwrap();
        let deserialized: T = serde_json::from_str(&serialized).unwrap();
        assert_eq!(format!("{:?}", msg), format!("{:?}", &deserialized));
    }

    // Adds a new variant to RawNetworkMessage and checks that it doesn't break deserialization of
    // old messages.
    #[test]
    fn test_backwards_compatibility() {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct SomeStruct {
            a: String,
            bc: u64,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        enum NewNetworkMessage {
            LocalInclusionList(InclusionListMessage),
            SharedInclusionList(InclusionListMessage),
            NewVariant(SomeStruct),
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(untagged)]
        enum NewRawNetworkMessage {
            /// Initial authentication message
            Hello(HelloMessage),
            /// Other, unauthenticated messages
            Other(NewNetworkMessage),
        }

        let new_message = NewRawNetworkMessage::Other(NewNetworkMessage::NewVariant(SomeStruct {
            a: "test".into(),
            bc: 123,
        }));
        // Sanity check that it works
        test_roundtrip(&new_message);

        // Check that message fails to be deserialized by old version
        // TODO: should we support this case?
        let serialized = serde_json::to_string(&new_message).unwrap();
        let deserialized: Result<RawNetworkMessage, _> = serde_json::from_str(&serialized);
        deserialized.unwrap_err();

        // Check that old messages still deserialize correctly
        let old_message = RawNetworkMessage::Other(NetworkMessage::LocalInclusionList(
            InclusionListMessage::new(123, InclusionList {
                txs: vec![Transaction(Bytes::from([0, 6, 5]))].into(),
            }),
        ));
        let serialized = serde_json::to_string(&old_message).unwrap();
        let _: NewRawNetworkMessage = serde_json::from_str(&serialized).unwrap();
    }

    // ### Unit tests for each variant ###

    #[test]
    fn test_hello_message_encoding() {
        let serialized_message = r#"{
            "pubkey": "0x8c9b2ed97d5879ef7df8458131b5c5ea3c8b55588d93c936274984ed9b24c65dbc35eb8f4ea72cdfb904f4b382a0973c",
            "valid_until": 1757622216306,
            "signature": "0xaa37e23d9ad2e987c27994f9c1e961bd06e866f8f531071f267e10fb98c5825e887710788ffa45b12c48ec1bad30588d0396e0d6dc54ee2197cd28ba912fd6e903495c99af368832c78875a2ef62218469d9936b3c94f8c52fc5195380681900",
            "supported_message_types": ["InclusionList"]
        }"#;
        let message: RawNetworkMessage = serde_json::from_str(&serialized_message).unwrap();
        assert!(matches!(message, RawNetworkMessage::Hello(_)));

        test_roundtrip(&message);
    }

    #[test]
    fn test_hello_message_encoding_unknown_message_type() {
        let serialized_message = r#"{
            "pubkey": "0x8c9b2ed97d5879ef7df8458131b5c5ea3c8b55588d93c936274984ed9b24c65dbc35eb8f4ea72cdfb904f4b382a0973c",
            "valid_until": 1757622216306,
            "signature": "0xaa37e23d9ad2e987c27994f9c1e961bd06e866f8f531071f267e10fb98c5825e887710788ffa45b12c48ec1bad30588d0396e0d6dc54ee2197cd28ba912fd6e903495c99af368832c78875a2ef62218469d9936b3c94f8c52fc5195380681900",
            "supported_message_types": ["InclusionList", "NewFeature"]
        }"#;
        let message: RawNetworkMessage = serde_json::from_str(&serialized_message).unwrap();
        assert!(matches!(message, RawNetworkMessage::Hello(_)));

        test_roundtrip(&message);
    }

    #[test]
    fn test_local_inclusion_list_encoding() {
        let serialized_message = r#"{
            "LocalInclusionList": {
                "slot": 12345,
                "inclusion_list": {
                    "txs": [
                        "0x6236236262362236f362362362a2626436575685967984232414",
                        "0x356723567235672f56723567235672a5672356723567235672356723567238"
                    ]
                }
            }
        }"#;
        let message: RawNetworkMessage = serde_json::from_str(&serialized_message).unwrap();
        assert!(matches!(message, RawNetworkMessage::Other(NetworkMessage::LocalInclusionList(_))));

        test_roundtrip(&message);
    }

    #[test]
    fn test_shared_inclusion_list_encoding() {
        let serialized_message = r#"{
            "SharedInclusionList": {
                "slot": 12345,
                "inclusion_list": {
                    "txs": [
                        "0x6236236262362236f362362362a2626436575685967984232414",
                        "0x356723567235672f56723567235672a5672356723567235672356723567238"
                    ]
                }
            }
        }"#;
        let message: RawNetworkMessage = serde_json::from_str(&serialized_message).unwrap();
        assert!(matches!(
            message,
            RawNetworkMessage::Other(NetworkMessage::SharedInclusionList(_))
        ));

        test_roundtrip(&message);
    }
}
