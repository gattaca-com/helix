use std::{
    io::Read,
    time::{Duration, Instant},
};

use bytes::Bytes;
use flate2::read::GzDecoder;
use helix_common::metrics::{
    BID_DECODING_LATENCY, BID_DECOMPRESS_SIZEHINT_REL_ERROR, DECOMPRESSION_LATENCY,
    SUBMISSION_BY_COMPRESSION, SUBMISSION_BY_ENCODING, SUBMISSION_COMPRESSED_BYTES,
    SUBMISSION_DECOMPRESSED_BYTES,
};
use helix_types::{BlsPublicKeyBytes, Compression, ForkName, ForkVersionDecode};
use http::HeaderMap;
use serde::de::DeserializeOwned;
use ssz::Decode;
use strum::{AsRefStr, EnumString};
use tracing::trace;
use zstd::{
    stream::read::Decoder as ZstdDecoder,
    zstd_safe::{CONTENTSIZE_ERROR, CONTENTSIZE_UNKNOWN, get_frame_content_size},
};

use crate::api::{
    HEADER_SUBMISSION_TYPE,
    builder::{api::MAX_PAYLOAD_LENGTH, error::BuilderApiError},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum SubmissionType {
    Default,
    Merge,
    Dehydrated,
}

impl SubmissionType {
    pub fn from_headers(header_map: &HeaderMap) -> Option<Self> {
        let submission_type = header_map.get(HEADER_SUBMISSION_TYPE)?.to_str().ok()?;
        submission_type.parse().ok()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Encoding {
    Json,
    Ssz,
}

#[derive(Debug)]
pub struct SubmissionDecoder {
    compression: Compression,
    encoding: Encoding,

    bytes_before_decompress: usize,
    bytes_after_decompress: usize,
    estimated_decompress: usize,

    decompress_latency: Duration,
    decode_latency: Duration,
}

impl SubmissionDecoder {
    pub fn new(compression: Compression, encoding: Encoding) -> Self {
        Self {
            compression,
            encoding,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        }
    }

    // TODO: we could also just extract the bid trace and send that through before the rest is
    // decoded after some light validation
    /// Assume buf is already decompressed
    pub fn extract_builder_pubkey(
        &self,
        buf: &[u8],
        has_mergeable_data: bool,
    ) -> Result<BlsPublicKeyBytes, BuilderApiError> {
        match self.encoding {
            Encoding::Json => {
                #[derive(serde::Deserialize)]
                struct Outer {
                    submission: Bid,
                }

                #[derive(serde::Deserialize)]
                struct Bid {
                    message: Message,
                }
                #[derive(serde::Deserialize)]
                struct Message {
                    builder_pubkey: BlsPublicKeyBytes,
                }

                let bid: Bid = if has_mergeable_data {
                    serde_json::from_slice::<Outer>(buf)?.submission
                } else {
                    serde_json::from_slice(buf)?
                };

                Ok(bid.message.builder_pubkey)
            }
            Encoding::Ssz => {
                const BUILDER_PUBKEY_OFFSET: usize = 8 + /* slot */
                32 + /* parent_hash */
                32; /* block_hash */

                if buf.len() < BUILDER_PUBKEY_OFFSET + BlsPublicKeyBytes::len_bytes() {
                    return Err(BuilderApiError::PayloadDecode);
                }

                let pubkey = unsafe {
                    core::ptr::read_unaligned(
                        buf.as_ptr().add(BUILDER_PUBKEY_OFFSET) as *const BlsPublicKeyBytes
                    )
                };

                Ok(pubkey)
            }
        }
    }

    pub fn decompress(&mut self, body: Bytes) -> Result<Bytes, BuilderApiError> {
        let start = Instant::now();
        self.bytes_before_decompress = body.len();
        let decompressed = match self.compression {
            Compression::None => body,
            Compression::Gzip => {
                let mut decoder = GzDecoder::new(body.as_ref());
                let cap = gzip_size_hint(&body).unwrap_or(body.len() * 2);
                self.estimated_decompress = cap;
                let mut buf = Vec::with_capacity(cap);

                decoder.read_to_end(&mut buf)?;
                buf.into()
            }
            Compression::Zstd => {
                let mut decoder = ZstdDecoder::new(body.as_ref())?;
                let cap = zstd_size_hint(&body).unwrap_or(body.len() * 2);
                self.estimated_decompress = cap;
                let mut buf = Vec::with_capacity(cap);

                decoder.read_to_end(&mut buf)?;
                buf.into()
            }
        };

        self.bytes_after_decompress = decompressed.len();
        self.decompress_latency = start.elapsed();

        Ok(decompressed)
    }

    // TODO: pass a buffer pool to avoid allocations
    pub fn decode<T: Decode + DeserializeOwned>(
        &mut self,
        body: Bytes,
    ) -> Result<T, BuilderApiError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => T::from_ssz_bytes(&body).map_err(BuilderApiError::SszDecode)?,
            Encoding::Json => serde_json::from_slice(&body)?,
        };

        self.decode_latency = start.elapsed().saturating_sub(self.decompress_latency);
        self.record_metrics();

        Ok(payload)
    }

    pub fn decode_by_fork<T: ForkVersionDecode + DeserializeOwned>(
        &mut self,
        body: Bytes,
        fork: ForkName,
    ) -> Result<T, BuilderApiError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => {
                T::from_ssz_bytes_by_fork(&body, fork).map_err(BuilderApiError::SszDecode)?
            }
            Encoding::Json => serde_json::from_slice(&body)?,
        };

        self.decode_latency = start.elapsed().saturating_sub(self.decompress_latency);
        self.record_metrics();

        Ok(payload)
    }

    fn record_metrics(&self) {
        let compression_label = self.compression.as_str();
        SUBMISSION_BY_COMPRESSION.with_label_values(&[compression_label]).inc();

        if self.compression != Compression::None {
            SUBMISSION_COMPRESSED_BYTES
                .with_label_values(&[compression_label])
                .inc_by(self.bytes_before_decompress as u64);
            SUBMISSION_DECOMPRESSED_BYTES
                .with_label_values(&[compression_label])
                .inc_by(self.bytes_after_decompress as u64);
            DECOMPRESSION_LATENCY
                .with_label_values(&[compression_label])
                .observe(self.decompress_latency.as_micros() as f64);

            if self.estimated_decompress > 0 {
                let actual = self.bytes_after_decompress as f64;
                let estimate = self.estimated_decompress as f64;
                let error = (actual - estimate).abs() / actual.max(1.0);
                BID_DECOMPRESS_SIZEHINT_REL_ERROR
                    .with_label_values(&[compression_label])
                    .observe(error)
            }
        }

        let encoding_label = match self.encoding {
            Encoding::Json => "json",
            Encoding::Ssz => "ssz",
        };
        SUBMISSION_BY_ENCODING.with_label_values(&[encoding_label]).inc();
        BID_DECODING_LATENCY
            .with_label_values(&[encoding_label])
            .observe(self.decode_latency.as_micros() as f64);

        trace!(
            size_compressed = self.bytes_before_decompress,
            size_uncompressed = self.bytes_after_decompress,
            compression =? self.compression,
            decode_latency =? self.decode_latency,
            "decoded payload"
        );
    }
}

fn zstd_size_hint(buf: &[u8]) -> Option<usize> {
    match get_frame_content_size(buf) {
        Ok(Some(size)) if size != CONTENTSIZE_ERROR && size != CONTENTSIZE_UNKNOWN => {
            Some((size as usize).min(MAX_PAYLOAD_LENGTH))
        }

        Ok(_) | Err(_) => None,
    }
}

fn gzip_size_hint(buf: &[u8]) -> Option<usize> {
    if buf.len() >= 4 {
        let isize = u32::from_le_bytes(buf[buf.len() - 4..].try_into().ok()?);
        Some((isize as usize).min(MAX_PAYLOAD_LENGTH))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex::FromHex;
    use helix_types::{
        MergeType, SignedBidSubmission, SignedBidSubmissionElectra,
        SignedBidSubmissionWithMergingData, TestRandomSeed,
    };
    use ssz::Encode;

    use super::*;

    #[test]
    fn test_get_builder_pubkey() {
        let expected = BlsPublicKeyBytes::from_hex("0xa1885d66bef164889a2e35845c3b626545d7b0e513efe335e97c3a45e534013fa3bc38c3b7e6143695aecc4872ac52c4").unwrap();

        let data_json =
            include_bytes!("../../../types/src/testdata/signed-bid-submission-electra-2.json");
        let decoder = SubmissionDecoder {
            compression: Compression::Gzip,
            encoding: Encoding::Json,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        };

        let pubkey = decoder.extract_builder_pubkey(data_json, false).unwrap();
        assert_eq!(pubkey, expected);

        let data_ssz =
            include_bytes!("../../../types/src/testdata/signed-bid-submission-electra-2.bin");
        let decoder = SubmissionDecoder {
            compression: Compression::Gzip,
            encoding: Encoding::Ssz,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        };

        let pubkey = decoder.extract_builder_pubkey(data_ssz, false).unwrap();
        assert_eq!(pubkey, expected);
    }

    #[test]
    fn test_get_builder_pubkey_merging() {
        let sub = SignedBidSubmissionElectra::test_random();
        let sub = SignedBidSubmission::Electra(sub);
        let sub = SignedBidSubmissionWithMergingData {
            submission: sub,
            merging_data: Default::default(),
        };

        let data_json = serde_json::to_vec(&sub).unwrap();
        let decoder = SubmissionDecoder {
            compression: Compression::Gzip,
            encoding: Encoding::Json,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        };

        let pubkey_json = decoder.extract_builder_pubkey(data_json.as_slice(), true).unwrap();

        let data_ssz = sub.as_ssz_bytes();
        let decoder = SubmissionDecoder {
            compression: Compression::Gzip,
            encoding: Encoding::Ssz,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        };

        let pubkey_ssz = decoder.extract_builder_pubkey(data_ssz.as_slice(), true).unwrap();

        assert_eq!(pubkey_json, pubkey_ssz)
    }

    #[test]
    fn test_submission_type_serialization() {
        assert_eq!(SubmissionType::Default.as_ref(), "default");
        assert_eq!(SubmissionType::Merge.as_ref(), "merge");
        assert_eq!(SubmissionType::Dehydrated.as_ref(), "dehydrated");
    }

    #[test]
    fn test_submission_type_deserialization() {
        assert_eq!("default".parse::<SubmissionType>().unwrap(), SubmissionType::Default);
        assert_eq!("merge".parse::<SubmissionType>().unwrap(), SubmissionType::Merge);
        assert_eq!("dehydrated".parse::<SubmissionType>().unwrap(), SubmissionType::Dehydrated);

        //Case shouldn't matter
        assert_eq!("Default".parse::<SubmissionType>().unwrap(), SubmissionType::Default);
        assert_eq!("Merge".parse::<SubmissionType>().unwrap(), SubmissionType::Merge);
        assert_eq!("Dehydrated".parse::<SubmissionType>().unwrap(), SubmissionType::Dehydrated);

        // Test that invalid values fail
        assert!("invalid".parse::<SubmissionType>().is_err());
        assert!("MergeAppendOnly".parse::<SubmissionType>().is_err()); // CamelCase should fail
    }

    #[test]
    fn test_merge_type_serialization() {
        assert_eq!(MergeType::Mergeable.as_ref(), "mergeable");
        assert_eq!(MergeType::AppendOnly.as_ref(), "append_only");
    }

    #[test]
    fn test_merge_type_deserialization() {
        assert_eq!("mergeable".parse::<MergeType>().unwrap(), MergeType::Mergeable);
        assert_eq!("append_only".parse::<MergeType>().unwrap(), MergeType::AppendOnly);

        //Case shouldn't matter
        assert_eq!("Mergeable".parse::<MergeType>().unwrap(), MergeType::Mergeable);
        assert_eq!("Append_Only".parse::<MergeType>().unwrap(), MergeType::AppendOnly);

        // Test that invalid values fail
        assert!("invalid".parse::<MergeType>().is_err());
        assert!("AppendOnly".parse::<MergeType>().is_err()); // CamelCase should fail
    }
}
