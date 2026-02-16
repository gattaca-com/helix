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
use helix_types::{BlsPublicKeyBytes, ForkName, ForkVersionDecode, SeqNum};
use http::{
    HeaderMap, HeaderValue,
    header::{CONTENT_ENCODING, CONTENT_TYPE},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use ssz::Decode;
use strum::{AsRefStr, EnumString};
use tracing::trace;
use zstd::{
    stream::read::Decoder as ZstdDecoder,
    zstd_safe::{CONTENTSIZE_ERROR, CONTENTSIZE_UNKNOWN, get_frame_content_size},
};

use crate::{
    api::{
        HEADER_API_KEY, HEADER_API_TOKEN, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_MERGE_TYPE,
        HEADER_SEQUENCE, HEADER_SUBMISSION_TYPE, HEADER_WITH_ADJUSTMENTS,
        builder::{api::MAX_PAYLOAD_LENGTH, error::BuilderApiError},
    },
    tcp_bid_recv::{BidSubmissionHeader, types::BidSubmissionFlags},
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

#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, EnumString, AsRefStr)]
#[strum(serialize_all = "snake_case", ascii_case_insensitive)]
pub enum MergeType {
    #[default]
    None = 0,
    Mergeable = 1,
    AppendOnly = 2,
}

impl MergeType {
    pub fn from_headers(header_map: &HeaderMap, sub_type: Option<SubmissionType>) -> Self {
        match header_map.get(HEADER_MERGE_TYPE) {
            None => {
                if sub_type.is_some_and(|sub_type| sub_type == SubmissionType::Merge) ||
                    matches!(header_map.get(HEADER_IS_MERGEABLE), Some(header) if header == HeaderValue::from_static("true"))
                {
                    MergeType::Mergeable
                } else {
                    MergeType::None
                }
            }
            Some(merge_type) => {
                merge_type.to_str().ok().and_then(|t| t.parse().ok()).unwrap_or_default()
            }
        }
    }

    pub fn is_some(&self) -> bool {
        *self != MergeType::None
    }
}

impl TryFrom<u8> for MergeType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Mergeable),
            2 => Ok(Self::AppendOnly),
            _ => Err(()),
        }
    }
}

#[repr(u8)]
#[derive(
    Debug, Eq, PartialEq, Clone, Copy, Serialize, Deserialize, Hash, PartialOrd, Ord, Default,
)]
pub enum Compression {
    #[default]
    None = 0,
    Gzip = 1,
    Zstd = 2,
}

impl Compression {
    pub fn string(&self) -> String {
        match &self {
            Compression::None => "none".into(),
            Compression::Gzip => "gzip".into(),
            Compression::Zstd => "zstd".into(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match &self {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        }
    }
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", match self {
            Compression::None => "NONE",
            Compression::Gzip => "GZIP",
            Compression::Zstd => "ZSTD",
        })
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

pub fn headers_map_to_bid_submission_header(
    headers: http::header::HeaderMap,
) -> (BidSubmissionHeader, Option<SeqNum>, Encoding, Compression, Option<String>) {
    let mut flags = BidSubmissionFlags::default();

    if matches!(headers.get(HEADER_WITH_ADJUSTMENTS), Some(header) if header == HeaderValue::from_static("true"))
    {
        flags.set(BidSubmissionFlags::WITH_ADJUSTMENTS, true);
    }
    if headers.get(HEADER_HYDRATE).is_some() {
        flags.set(BidSubmissionFlags::IS_DEHYDRATED, true);
    }

    let submission_type = SubmissionType::from_headers(&headers);
    if submission_type.is_some_and(|sub_type| sub_type == SubmissionType::Dehydrated) {
        flags.set(BidSubmissionFlags::IS_DEHYDRATED, true);
    }

    let sequence_number = headers
        .get(HEADER_SEQUENCE)
        .and_then(|seq| seq.to_str().ok())
        .and_then(|seq| seq.parse::<u128>().ok());

    const GZIP_HEADER: HeaderValue = HeaderValue::from_static("gzip");
    const ZSTD_HEADER: HeaderValue = HeaderValue::from_static("zstd");

    let compression = match headers.get(CONTENT_ENCODING) {
        Some(header) if header == GZIP_HEADER => Compression::Gzip,
        Some(header) if header == ZSTD_HEADER => Compression::Zstd,
        _ => Compression::None,
    };

    const SSZ_HEADER: HeaderValue = HeaderValue::from_static("application/octet-stream");

    let encoding = match headers.get(CONTENT_TYPE) {
        Some(header) if header == SSZ_HEADER => Encoding::Ssz,
        _ => Encoding::Json,
    };

    let merge_type = MergeType::from_headers(&headers, submission_type);

    let api_key = headers
        .get(HEADER_API_KEY)
        .or(headers.get(HEADER_API_TOKEN))
        .and_then(|key| key.to_str().map(|key| key.to_owned()).ok());

    let header = BidSubmissionHeader {
        sequence_number: sequence_number.unwrap_or_default(),
        merge_type,
        flags,
    };

    (header, sequence_number, encoding, compression, api_key)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex::FromHex;
    use helix_types::{
        SignedBidSubmission, SignedBidSubmissionElectra, SignedBidSubmissionWithMergingData,
        TestRandomSeed,
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
