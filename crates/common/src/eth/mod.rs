pub mod bellatrix;
pub mod capella;
pub mod deneb;
pub mod signed_proposal;
pub mod versioned_payload;
pub mod versioned_payload_header;

use ethereum_consensus::{
    crypto::SecretKey,
    primitives::{BlsPublicKey, Hash32, Slot},
    ssz::prelude::*,
    state_transition::Context,
    types::mainnet::{ExecutionPayload, ExecutionPayloadHeader},
    Error,
};

use helix_utils::signing::sign_builder_message;
use serde::de;

use crate::{
    bid_submission::{
        v2::header_submission::SignedHeaderSubmission, BidSubmission, SignedBidSubmission,
    },
    proofs::InclusionProofs,
};

/// Index of the `blob_kzg_commitments` leaf in the `BeaconBlockBody` tree post-deneb.
pub const BLOB_KZG_COMMITMENTS_INDEX: usize = 11;

#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BidRequest {
    #[serde(with = "ethereum_consensus::serde::as_str")]
    pub slot: Slot,
    pub parent_hash: Hash32,
    pub public_key: BlsPublicKey,
}

impl std::fmt::Display for BidRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slot = self.slot;
        let parent_hash = &self.parent_hash;
        let public_key = &self.public_key;
        write!(f, "slot {slot}, parent hash {parent_hash} and proposer {public_key}")
    }
}

/// A signed builder bid with optional inclusion proofs.
///
/// Deserialized from a JSON object of the following format:
///
/// ```json
/// {
///     "version": "deneb",
///     "data": {
///         "message": BuilderBid,
///         "signature": Signature,
///         "proofs": Option<InclusionProofs>
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum SignedBuilderBid {
    Bellatrix(bellatrix::SignedBuilderBid, Option<InclusionProofs>),
    Capella(capella::SignedBuilderBid, Option<InclusionProofs>),
    Deneb(deneb::SignedBuilderBid, Option<InclusionProofs>),
}

impl<'de> serde::Deserialize<'de> for SignedBuilderBid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let data = value.get("data").ok_or_else(|| de::Error::custom("missing data"))?;
        let version = value.get("version").ok_or_else(|| de::Error::custom("missing version"))?;

        // deserialize proofs if they exist, from the "data" field
        let proofs = data
            .get("proofs")
            .map(|proofs| {
                <InclusionProofs as serde::Deserialize>::deserialize(proofs)
                    .map_err(de::Error::custom)
            })
            .transpose()?;

        // deserialize data into SignedBuilderBid based on its version
        match version.as_str().ok_or_else(|| de::Error::custom("version is not a string"))? {
            "bellatrix" => {
                let bid = <bellatrix::SignedBuilderBid as serde::Deserialize>::deserialize(data)
                    .map_err(de::Error::custom)?;
                Ok(SignedBuilderBid::Bellatrix(bid, proofs))
            }
            "capella" => {
                let bid = <capella::SignedBuilderBid as serde::Deserialize>::deserialize(data)
                    .map_err(de::Error::custom)?;
                Ok(SignedBuilderBid::Capella(bid, proofs))
            }
            "deneb" => {
                let bid = <deneb::SignedBuilderBid as serde::Deserialize>::deserialize(data)
                    .map_err(de::Error::custom)?;
                Ok(SignedBuilderBid::Deneb(bid, proofs))
            }
            _ => Err(de::Error::custom("unknown version")),
        }
    }
}

impl serde::Serialize for SignedBuilderBid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Bellatrix(bid, proofs) => {
                let mut map = serde_json::Map::new();
                map.insert("version".to_string(), "bellatrix".into());

                let mut data_map = serde_json::Map::new();
                data_map.insert("message".to_string(), serde_json::to_value(&bid.message).unwrap());
                data_map
                    .insert("signature".to_string(), serde_json::to_value(&bid.signature).unwrap());
                if let Some(proofs) = proofs {
                    data_map.insert("proofs".to_string(), serde_json::to_value(proofs).unwrap());
                }

                map.insert("data".to_string(), serde_json::to_value(data_map).unwrap());
                map.serialize(serializer)
            }
            Self::Capella(bid, proofs) => {
                let mut map = serde_json::Map::new();
                map.insert("version".to_string(), "capella".into());

                let mut data_map = serde_json::Map::new();
                data_map.insert("message".to_string(), serde_json::to_value(&bid.message).unwrap());
                data_map
                    .insert("signature".to_string(), serde_json::to_value(&bid.signature).unwrap());
                if let Some(proofs) = proofs {
                    data_map.insert("proofs".to_string(), serde_json::to_value(proofs).unwrap());
                }

                map.insert("data".to_string(), serde_json::to_value(data_map).unwrap());
                map.serialize(serializer)
            }
            Self::Deneb(bid, proofs) => {
                let mut map = serde_json::Map::new();
                map.insert("version".to_string(), "deneb".into());

                let mut data_map = serde_json::Map::new();
                data_map.insert("message".to_string(), serde_json::to_value(&bid.message).unwrap());
                data_map
                    .insert("signature".to_string(), serde_json::to_value(&bid.signature).unwrap());
                if let Some(proofs) = proofs {
                    data_map.insert("proofs".to_string(), serde_json::to_value(proofs).unwrap());
                }

                map.insert("data".to_string(), serde_json::to_value(data_map).unwrap());
                map.serialize(serializer)
            }
        }
    }
}

impl std::fmt::Display for SignedBuilderBid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let block_hash = self.block_hash();
        let value = self.value();
        write!(f, "block hash {block_hash} and value {value:?}")
    }
}

impl SignedBuilderBid {
    pub fn from_submission(
        submission: &mut SignedBidSubmission,
        proofs: Option<InclusionProofs>,
        public_key: BlsPublicKey,
        signing_key: &SecretKey,
        context: &Context,
    ) -> Result<Self, Error> {
        match &mut submission.execution_payload_mut() {
            ExecutionPayload::Bellatrix(payload) => {
                let header = bellatrix::ExecutionPayloadHeader::try_from(payload)?;
                let mut message =
                    bellatrix::BuilderBid { header, value: submission.value(), public_key };
                let signature = sign_builder_message(&mut message, signing_key, context)?;

                Ok(Self::Bellatrix(bellatrix::SignedBuilderBid { message, signature }, proofs))
            }
            ExecutionPayload::Capella(payload) => {
                let header = capella::ExecutionPayloadHeader::try_from(payload)?;
                let mut message =
                    capella::BuilderBid { header, value: submission.value(), public_key };
                let signature = sign_builder_message(&mut message, signing_key, context)?;
                Ok(Self::Capella(capella::SignedBuilderBid { message, signature }, proofs))
            }
            ExecutionPayload::Deneb(payload) => {
                let header = deneb::ExecutionPayloadHeader::try_from(payload)?;
                match submission.blobs_bundle() {
                    Some(blobs_bundle) => {
                        let mut message = deneb::BuilderBid {
                            header,
                            blob_kzg_commitments: blobs_bundle.commitments.clone(),
                            value: submission.value(),
                            public_key,
                        };
                        let signature = sign_builder_message(&mut message, signing_key, context)?;

                        Ok(Self::Deneb(deneb::SignedBuilderBid { message, signature }, proofs))
                    }
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "Missing blobs bundle",
                    )
                    .into()),
                }
            }
        }
    }

    pub fn from_header_submission(
        submission: &SignedHeaderSubmission,
        proofs: Option<InclusionProofs>,
        public_key: BlsPublicKey,
        signing_key: &SecretKey,
        context: &Context,
    ) -> Result<Self, Error> {
        match &submission.execution_payload_header() {
            ExecutionPayloadHeader::Bellatrix(header) => {
                let mut message = bellatrix::BuilderBid {
                    header: header.clone(),
                    value: submission.value(),
                    public_key,
                };
                let signature = sign_builder_message(&mut message, signing_key, context)?;

                Ok(Self::Bellatrix(bellatrix::SignedBuilderBid { message, signature }, proofs))
            }
            ExecutionPayloadHeader::Capella(header) => {
                let mut message = capella::BuilderBid {
                    header: header.clone(),
                    value: submission.value(),
                    public_key,
                };
                let signature = sign_builder_message(&mut message, signing_key, context)?;
                Ok(Self::Capella(capella::SignedBuilderBid { message, signature }, proofs))
            }
            ExecutionPayloadHeader::Deneb(header) => match submission.commitments() {
                Some(commitments) => {
                    let mut message = deneb::BuilderBid {
                        header: header.clone(),
                        blob_kzg_commitments: commitments.clone(),
                        value: submission.value(),
                        public_key,
                    };
                    let signature = sign_builder_message(&mut message, signing_key, context)?;

                    Ok(Self::Deneb(deneb::SignedBuilderBid { message, signature }, proofs))
                }
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Missing blobs bundle",
                )
                .into()),
            },
        }
    }

    pub fn value(&self) -> U256 {
        match self {
            Self::Bellatrix(bid, _) => bid.message.value,
            Self::Capella(bid, _) => bid.message.value,
            Self::Deneb(bid, _) => bid.message.value,
        }
    }

    pub fn public_key(&self) -> &BlsPublicKey {
        match self {
            Self::Bellatrix(bid, _) => &bid.message.public_key,
            Self::Capella(bid, _) => &bid.message.public_key,
            Self::Deneb(bid, _) => &bid.message.public_key,
        }
    }

    pub fn block_hash(&self) -> &Hash32 {
        match self {
            Self::Bellatrix(bid, _) => &bid.message.header.block_hash,
            Self::Capella(bid, _) => &bid.message.header.block_hash,
            Self::Deneb(bid, _) => &bid.message.header.block_hash,
        }
    }

    pub fn parent_hash(&self) -> &Hash32 {
        match self {
            Self::Bellatrix(bid, _) => &bid.message.header.parent_hash,
            Self::Capella(bid, _) => &bid.message.header.parent_hash,
            Self::Deneb(bid, _) => &bid.message.header.parent_hash,
        }
    }

    pub fn logs_bloom(&self) -> &ByteVector<256> {
        match self {
            Self::Bellatrix(bid, _) => &bid.message.header.logs_bloom,
            Self::Capella(bid, _) => &bid.message.header.logs_bloom,
            Self::Deneb(bid, _) => &bid.message.header.logs_bloom,
        }
    }

    pub fn version(&self) -> &str {
        match self {
            Self::Bellatrix(_, _) => "bellatrix",
            Self::Capella(_, _) => "capella",
            Self::Deneb(_, _) => "deneb",
        }
    }

    pub fn proofs(&self) -> &Option<InclusionProofs> {
        match self {
            Self::Bellatrix(_, proofs) => proofs,
            Self::Capella(_, proofs) => proofs,
            Self::Deneb(_, proofs) => proofs,
        }
    }

    pub fn set_inclusion_proofs(&mut self, proofs: InclusionProofs) {
        match self {
            Self::Bellatrix(_, proofs_opt) => *proofs_opt = Some(proofs),
            Self::Capella(_, proofs_opt) => *proofs_opt = Some(proofs),
            Self::Deneb(_, proofs_opt) => *proofs_opt = Some(proofs),
        }
    }
}

pub fn try_execution_header_from_payload(
    execution_payload: &mut ExecutionPayload,
) -> Result<ExecutionPayloadHeader, Error> {
    match execution_payload {
        ExecutionPayload::Bellatrix(execution_payload) => {
            let header = bellatrix::ExecutionPayloadHeader::try_from(execution_payload)?;
            Ok(ExecutionPayloadHeader::Bellatrix(header))
        }
        ExecutionPayload::Capella(execution_payload) => {
            let header = capella::ExecutionPayloadHeader::try_from(execution_payload)?;
            Ok(ExecutionPayloadHeader::Capella(header))
        }
        ExecutionPayload::Deneb(execution_payload) => {
            let header = deneb::ExecutionPayloadHeader::try_from(execution_payload)?;
            Ok(ExecutionPayloadHeader::Deneb(header))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ethereum_consensus::types::mainnet::SignedBlindedBeaconBlock;

    #[test]
    fn build_signed_builder() {
        let x = capella::SignedBuilderBid {
            message: Default::default(),
            signature: Default::default(),
        };
        let x = SignedBuilderBid::Capella(x, None);

        let x = serde_json::to_vec(&x).unwrap();
        println!("{:?}", x);
    }

    #[test]
    fn test_deserialize_signed_blinded_beacon_block() {
        let json_bytes = vec![
            123, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 123, 34, 115, 108, 111, 116, 34, 58,
            34, 50, 49, 34, 44, 34, 112, 114, 111, 112, 111, 115, 101, 114, 95, 105, 110, 100, 101,
            120, 34, 58, 34, 50, 34, 44, 34, 112, 97, 114, 101, 110, 116, 95, 114, 111, 111, 116,
            34, 58, 34, 48, 120, 48, 50, 48, 51, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 34, 44, 34, 115, 116, 97, 116, 101, 95, 114, 111, 111, 116, 34, 58, 34,
            48, 120, 49, 55, 48, 52, 48, 52, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 34, 44, 34, 98, 111, 100, 121, 34, 58, 123, 34, 114, 97, 110, 100, 97, 111, 95,
            114, 101, 118, 101, 97, 108, 34, 58, 34, 48, 120, 48, 52, 50, 50, 48, 53, 48, 54, 48,
            53, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 101, 116, 104, 49, 95, 100, 97, 116, 97, 34,
            58, 123, 34, 100, 101, 112, 111, 115, 105, 116, 95, 114, 111, 111, 116, 34, 58, 34, 48,
            120, 51, 54, 49, 55, 48, 98, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            34, 44, 34, 100, 101, 112, 111, 115, 105, 116, 95, 99, 111, 117, 110, 116, 34, 58, 34,
            50, 51, 34, 44, 34, 98, 108, 111, 99, 107, 95, 104, 97, 115, 104, 34, 58, 34, 48, 120,
            48, 53, 48, 54, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34,
            125, 44, 34, 103, 114, 97, 102, 102, 105, 116, 105, 34, 58, 34, 48, 120, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 112,
            114, 111, 112, 111, 115, 101, 114, 95, 115, 108, 97, 115, 104, 105, 110, 103, 115, 34,
            58, 91, 93, 44, 34, 97, 116, 116, 101, 115, 116, 101, 114, 95, 115, 108, 97, 115, 104,
            105, 110, 103, 115, 34, 58, 91, 93, 44, 34, 97, 116, 116, 101, 115, 116, 97, 116, 105,
            111, 110, 115, 34, 58, 91, 93, 44, 34, 100, 101, 112, 111, 115, 105, 116, 115, 34, 58,
            91, 93, 44, 34, 118, 111, 108, 117, 110, 116, 97, 114, 121, 95, 101, 120, 105, 116,
            115, 34, 58, 91, 93, 44, 34, 115, 121, 110, 99, 95, 97, 103, 103, 114, 101, 103, 97,
            116, 101, 34, 58, 123, 34, 115, 121, 110, 99, 95, 99, 111, 109, 109, 105, 116, 116,
            101, 101, 95, 98, 105, 116, 115, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 115, 121, 110, 99, 95, 99, 111,
            109, 109, 105, 116, 116, 101, 101, 95, 115, 105, 103, 110, 97, 116, 117, 114, 101, 34,
            58, 34, 48, 120, 48, 49, 54, 51, 54, 51, 54, 51, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34,
            125, 44, 34, 101, 120, 101, 99, 117, 116, 105, 111, 110, 95, 112, 97, 121, 108, 111,
            97, 100, 95, 104, 101, 97, 100, 101, 114, 34, 58, 123, 34, 112, 97, 114, 101, 110, 116,
            95, 104, 97, 115, 104, 34, 58, 34, 48, 120, 48, 49, 48, 49, 48, 50, 48, 51, 50, 100,
            48, 57, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 102, 101, 101, 95, 114, 101, 99,
            105, 112, 105, 101, 110, 116, 34, 58, 34, 48, 120, 48, 50, 48, 50, 48, 50, 48, 50, 49,
            55, 48, 52, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 115, 116, 97, 116, 101, 95, 114, 111,
            111, 116, 34, 58, 34, 48, 120, 48, 51, 51, 54, 48, 55, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 34, 44, 34, 114, 101, 99, 101, 105, 112, 116, 115, 95, 114,
            111, 111, 116, 34, 58, 34, 48, 120, 48, 56, 48, 55, 48, 53, 49, 55, 48, 50, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 108, 111, 103, 115, 95, 98, 108, 111, 111, 109,
            34, 58, 34, 48, 120, 100, 101, 50, 49, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 112, 114, 101, 118, 95,
            114, 97, 110, 100, 97, 111, 34, 58, 34, 48, 120, 48, 98, 52, 100, 52, 50, 50, 98, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 98, 108, 111, 99, 107, 95, 110,
            117, 109, 98, 101, 114, 34, 58, 34, 49, 34, 44, 34, 103, 97, 115, 95, 108, 105, 109,
            105, 116, 34, 58, 34, 50, 34, 44, 34, 103, 97, 115, 95, 117, 115, 101, 100, 34, 58, 34,
            52, 34, 44, 34, 116, 105, 109, 101, 115, 116, 97, 109, 112, 34, 58, 34, 53, 34, 44, 34,
            101, 120, 116, 114, 97, 95, 100, 97, 116, 97, 34, 58, 34, 48, 120, 53, 52, 54, 57, 55,
            52, 54, 49, 54, 101, 34, 44, 34, 98, 97, 115, 101, 95, 102, 101, 101, 95, 112, 101,
            114, 95, 103, 97, 115, 34, 58, 34, 50, 53, 51, 34, 44, 34, 98, 108, 111, 99, 107, 95,
            104, 97, 115, 104, 34, 58, 34, 48, 120, 49, 100, 52, 100, 51, 55, 52, 50, 52, 55, 50,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 116, 114, 97, 110, 115, 97, 99, 116,
            105, 111, 110, 115, 95, 114, 111, 111, 116, 34, 58, 34, 48, 120, 48, 49, 48, 49, 48,
            49, 48, 50, 54, 51, 53, 56, 52, 100, 53, 56, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 119, 105,
            116, 104, 100, 114, 97, 119, 97, 108, 115, 95, 114, 111, 111, 116, 34, 58, 34, 48, 120,
            53, 52, 51, 54, 49, 55, 53, 54, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34,
            125, 44, 34, 98, 108, 115, 95, 116, 111, 95, 101, 120, 101, 99, 117, 116, 105, 111,
            110, 95, 99, 104, 97, 110, 103, 101, 115, 34, 58, 91, 93, 125, 125, 44, 34, 115, 105,
            103, 110, 97, 116, 117, 114, 101, 34, 58, 34, 48, 120, 48, 50, 48, 51, 48, 52, 48, 53,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 34, 125,
        ];
        match serde_json::from_slice::<SignedBlindedBeaconBlock>(&json_bytes) {
            Ok(res) => {
                println!("PASSED. slot: {}", res.message().slot())
            }
            Err(err) => {
                println!("ERROR! {err:?}");
            }
        }
    }

    #[test]
    fn test_serialization_of_signed_builder_bid() {
        let mut logs_bloom = ByteVector::<256>::default();
        logs_bloom[5] = 22;
        logs_bloom[0] = 255;

        let header = capella::ExecutionPayloadHeader {
            parent_hash: Default::default(),
            fee_recipient: Default::default(),
            state_root: Default::default(),
            receipts_root: Default::default(),
            logs_bloom,
            prev_randao: Default::default(),
            block_number: 111,
            gas_limit: 23,
            gas_used: 54232,
            timestamp: 11111,
            extra_data: Default::default(),
            base_fee_per_gas: Default::default(),
            block_hash: Default::default(),
            transactions_root: Default::default(),
            withdrawals_root: Default::default(),
        };

        let builder_bid = SignedBuilderBid::Capella(
            capella::SignedBuilderBid {
                message: capella::BuilderBid {
                    header,
                    value: U256::from(111111),
                    public_key: Default::default(),
                },
                signature: Default::default(),
            },
            None,
        );

        let serialized = serde_json::to_vec(&builder_bid);
        assert!(serialized.is_ok());
        let serialized = serialized.unwrap();

        let res = serde_json::from_slice::<SignedBuilderBid>(&serialized);
        assert!(res.is_ok());
    }

    #[test]
    fn test_deserialize_signed_builder_bid() {
        let json_bytes = vec![
            123, 34, 118, 101, 114, 115, 105, 111, 110, 34, 58, 34, 99, 97, 112, 101, 108, 108, 97,
            34, 44, 34, 100, 97, 116, 97, 34, 58, 123, 34, 109, 101, 115, 115, 97, 103, 101, 34,
            58, 123, 34, 104, 101, 97, 100, 101, 114, 34, 58, 123, 34, 112, 97, 114, 101, 110, 116,
            95, 104, 97, 115, 104, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 102, 101, 101, 95, 114, 101, 99, 105,
            112, 105, 101, 110, 116, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 115, 116, 97, 116, 101, 95, 114, 111, 111,
            116, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 34, 44, 34, 114, 101, 99, 101, 105, 112, 116, 115, 95, 114, 111,
            111, 116, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 34, 44, 34, 108, 111, 103, 115, 95, 98, 108, 111, 111, 109, 34,
            58, 34, 48, 120, 102, 102, 48, 48, 48, 48, 48, 48, 48, 48, 49, 54, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 112, 114, 101, 118, 95, 114,
            97, 110, 100, 97, 111, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 98, 108, 111, 99, 107, 95, 110, 117,
            109, 98, 101, 114, 34, 58, 34, 48, 34, 44, 34, 103, 97, 115, 95, 108, 105, 109, 105,
            116, 34, 58, 34, 50, 51, 34, 44, 34, 103, 97, 115, 95, 117, 115, 101, 100, 34, 58, 34,
            53, 52, 50, 51, 50, 34, 44, 34, 116, 105, 109, 101, 115, 116, 97, 109, 112, 34, 58, 34,
            49, 49, 49, 49, 49, 34, 44, 34, 101, 120, 116, 114, 97, 95, 100, 97, 116, 97, 34, 58,
            34, 48, 120, 34, 44, 34, 98, 97, 115, 101, 95, 102, 101, 101, 95, 112, 101, 114, 95,
            103, 97, 115, 34, 58, 34, 48, 34, 44, 34, 98, 108, 111, 99, 107, 95, 104, 97, 115, 104,
            34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 34, 44, 34, 116, 114, 97, 110, 115, 97, 99, 116, 105, 111, 110, 115,
            95, 114, 111, 111, 116, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 34, 119, 105, 116, 104, 100, 114, 97,
            119, 97, 108, 115, 95, 114, 111, 111, 116, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 125, 44, 34, 118, 97, 108,
            117, 101, 34, 58, 34, 49, 49, 49, 49, 49, 49, 34, 44, 34, 112, 117, 98, 107, 101, 121,
            34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 125, 44, 34, 115, 105, 103,
            110, 97, 116, 117, 114, 101, 34, 58, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 34, 125, 125,
        ];
        let res = serde_json::from_slice::<SignedBuilderBid>(&json_bytes);
        assert!(res.is_ok());
    }

    #[test]
    fn test_deserialize_json_signed_builder_bid_with_proofs() {
        let json = serde_json::json!({
            "version": "deneb",
            "data": {
                "message": {
                    "header": {
                        "parent_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                        "state_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "receipts_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
                        "prev_randao": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "block_number": "1",
                        "gas_limit": "1",
                        "gas_used": "1",
                        "timestamp": "1",
                        "extra_data": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "base_fee_per_gas": "1",
                        "blob_gas_used": "1",
                        "excess_blob_gas": "1",
                        "block_hash": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "transactions_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2",
                        "withdrawals_root": "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"
                    },
                    "blob_kzg_commitments": [
                        "0xa94170080872584e54a1cf092d845703b13907f2e6b3b1c0ad573b910530499e3bcd48c6378846b80d2bfa58c81cf3d5"
                    ],
                    "value": "1",
                    "pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
                },
                "proofs": {
                    "transaction_hashes": ["0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2", "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"],
                    "generalized_indexes": [4, 5],
                    "merkle_hashes": ["0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2", "0xcf8e0d4e9587369b2301d0790347320302cc0943d5a1884560367e8208d920f2"]
                },
                "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
            }
        });

        let res = serde_json::from_value::<SignedBuilderBid>(json);
        assert!(res.is_ok());

        let signed_builder_bid = res.unwrap();
        assert_eq!(signed_builder_bid.version(), "deneb");
        assert!(matches!(signed_builder_bid, SignedBuilderBid::Deneb(_, Some(_))));
    }
}
