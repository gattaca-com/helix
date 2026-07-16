use libp2p::identity::{PublicKey, secp256k1};
use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error};

/// Serialize a secp256k1 pubkey to hex.
pub(super) fn serialize_pubkey<S>(value: &PublicKey, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let key = value.clone().try_into_secp256k1().map_err(|e| S::Error::custom(e.to_string()))?;
    serializer.serialize_str(&hex::encode(&key.to_bytes()))
}

/// Deserialize a hex-encoded scp256k1 pubkey.
pub(super) fn deserialize_pubkey<'de, D>(d: D) -> Result<PublicKey, D::Error>
where
    D: Deserializer<'de>,
{
    let hex = String::deserialize(d)?;
    let bytes = hex::decode(hex).map_err(|e| D::Error::custom(e.to_string()))?;
    let secp_pubkey = secp256k1::PublicKey::try_from_bytes(&bytes)
        .map_err(|e| D::Error::custom(e.to_string()))?;
    Ok(PublicKey::from(secp_pubkey))
}
