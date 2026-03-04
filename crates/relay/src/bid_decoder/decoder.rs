use std::{
    io::Read,
    time::{Duration, Instant},
};

use bytes::Bytes;
use flate2::read::GzDecoder;
use helix_common::{SubmissionTrace, chain_info::ChainInfo, metrics::{
    BID_DECODING_LATENCY, BID_DECOMPRESS_SIZEHINT_REL_ERROR, DECOMPRESSION_LATENCY,
    SUBMISSION_BY_COMPRESSION, SUBMISSION_BY_ENCODING, SUBMISSION_COMPRESSED_BYTES,
    SUBMISSION_DECOMPRESSED_BYTES,
}, record_submission_step, utils::utcnow_ns};
use helix_types::{BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, Compression, DehydratedBidSubmission, DehydratedBidSubmissionFuluWithAdjustments, ForkName, ForkVersionDecode, MergeType, SignedBidSubmission, SignedBidSubmissionWithAdjustments, SignedBidSubmissionWithMergingData};
use http::{
    HeaderMap, HeaderValue,
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use ssz::Decode;
use strum::{AsRefStr, EnumString};
use tracing::{error, trace};
use zstd::{
    stream::read::Decoder as ZstdDecoder,
    zstd_safe::{CONTENTSIZE_ERROR, CONTENTSIZE_UNKNOWN, get_frame_content_size},
};

use crate::{api::{
    HEADER_SUBMISSION_TYPE,
    builder::{api::MAX_PAYLOAD_LENGTH, error::BuilderApiError},
}, auctioneer::Submission};

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

pub const HEADER_SSZ: &str = "application/octet-stream";
const HEADER_ACCEPT_SSZ: &str = "application/octet-stream;q=1.0,application/json;q=0.9";

impl Encoding {
    pub fn from_content_type(headers: &HeaderMap) -> Self {
        match headers.get(CONTENT_TYPE) {
            Some(header) if header == HeaderValue::from_static(HEADER_SSZ) => Encoding::Ssz,
            _ => Encoding::Json,
        }
    }

    pub fn from_accept(headers: &HeaderMap) -> Self {
        match headers.get(ACCEPT) {
            Some(header)
                if header == HeaderValue::from_static(HEADER_SSZ) ||
                    header == HeaderValue::from_static(HEADER_ACCEPT_SSZ) =>
            {
                Encoding::Ssz
            }
            _ => Encoding::Json,
        }
    }
}

pub(super) struct DecodeFlags {
    pub(super) skip_sigverify: bool,
    pub(super) merge_type: MergeType,
    pub(super) with_adjustments: bool,
    pub(super) block_merging_dry_run: bool,
}

pub(super) fn decode_dehydrated(
    decoder: &mut SubmissionDecoder,
    body: &bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    if !flags.skip_sigverify {
        return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
    }

    let (submission, bid_adjustment) = if flags.with_adjustments {
        let sub_with_adjustment: DehydratedBidSubmissionFuluWithAdjustments =
            decoder.decode_by_fork(body, chain_info.current_fork_name())?;
        let (sub, adjustment_data) = sub_with_adjustment.split();

        (sub, Some(adjustment_data))
    } else {
        let submission: DehydratedBidSubmission =
            decoder.decode_by_fork(body, chain_info.current_fork_name())?;

        (submission, None)
    };

    trace.decoded_ns = utcnow_ns();

    let merging_data = match flags.merge_type {
        MergeType::Mergeable => {
            //Should this return an error instead?
            error!("mergeable dehydrated submissions are not supported");
            None
        }
        MergeType::AppendOnly => Some(BlockMergingData::append_only(submission.fee_recipient())),
        MergeType::None => {
            if flags.block_merging_dry_run {
                Some(BlockMergingData::append_only(submission.fee_recipient()))
            } else {
                None
            }
        }
    };

    Ok((Submission::Dehydrated(submission), merging_data, bid_adjustment))
}

pub(super) fn decode_merge(
    decoder: &mut SubmissionDecoder,
    body: &bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    let sub_with_merging: SignedBidSubmissionWithMergingData = decoder.decode(body)?;
    let mut upgraded = sub_with_merging.maybe_upgrade_to_fulu(chain_info.current_fork_name());
    trace.decoded_ns = utcnow_ns();
    let merging_data = match flags.merge_type {
        MergeType::Mergeable => Some(upgraded.merging_data),
        //Handle append-only by creating empty mergeable orders
        //this allows builder to switch between append-only and mergeable without changing
        // submission alternatively we could reject or ignore append-only here if the
        // submission is mergeable?
        MergeType::AppendOnly => Some(BlockMergingData {
            allow_appending: upgraded.merging_data.allow_appending,
            builder_address: upgraded.merging_data.builder_address,
            merge_orders: vec![],
        }),
        MergeType::None => Some(upgraded.merging_data),
    };
    verify_and_validate(&mut upgraded.submission, flags.skip_sigverify, chain_info)?;
    Ok((Submission::Full(upgraded.submission), merging_data, None))
}

pub(super) fn decode_default(
    decoder: &mut SubmissionDecoder,
    body: &bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    let (submission, bid_adjustment) = if flags.with_adjustments {
        let sub_with_adjustment: SignedBidSubmissionWithAdjustments = decoder.decode(body)?;
        let (sub, adjustment_data) = sub_with_adjustment.split();

        (sub, Some(adjustment_data))
    } else {
        let submission: SignedBidSubmission = decoder.decode(body)?;

        (submission, None)
    };

    let mut upgraded = submission.maybe_upgrade_to_fulu(chain_info.current_fork_name());
    trace.decoded_ns = utcnow_ns();
    let merging_data = match flags.merge_type {
        MergeType::Mergeable => {
            //Should this return an error instead?
            error!("mergeable dehydrated submissions are not supported");
            None
        }
        MergeType::AppendOnly => Some(BlockMergingData::append_only(upgraded.fee_recipient())),
        MergeType::None => {
            if flags.block_merging_dry_run {
                Some(BlockMergingData::allow_all(upgraded.fee_recipient(), upgraded.num_txs()))
            } else {
                None
            }
        }
    };
    verify_and_validate(&mut upgraded, flags.skip_sigverify, chain_info)?;
    Ok((Submission::Full(upgraded), merging_data, bid_adjustment))
}

fn verify_and_validate(
    submission: &mut SignedBidSubmission,
    skip_sigverify: bool,
    chain_info: &ChainInfo,
) -> Result<(), BuilderApiError> {
    if !skip_sigverify {
        trace!("verifying signature");
        let start_sig = Instant::now();
        submission.verify_signature(chain_info.builder_domain)?;
        trace!("signature ok");
        record_submission_step("signature", start_sig.elapsed());
    }
    submission.validate_payload_ssz_lengths(chain_info.max_blobs_per_block())?;
    Ok(())
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

    pub fn decompress(&mut self, body: &Bytes) -> Option<Result<Bytes, BuilderApiError>> {
        let start = Instant::now();
        self.bytes_before_decompress = body.len();
        let decompressed: Bytes = match self.compression {
            Compression::None => {
                return None;
            }
            Compression::Gzip => {
                let mut decoder = GzDecoder::new(body.as_ref());
                let cap = gzip_size_hint(body).unwrap_or(body.len() * 2);
                self.estimated_decompress = cap;
                let mut buf = Vec::with_capacity(cap);

                if let Err(e) = decoder.read_to_end(&mut buf) {
                    return Some(Err(e.into()));
                }

                buf.into()
            }
            Compression::Zstd => {
                let mut decoder = match ZstdDecoder::new(body.as_ref()) {
                    Ok(d) => d,
                    Err(e) => {
                        return Some(Err(e.into()));
                    }
                };

                let cap = zstd_size_hint(body).unwrap_or(body.len() * 2);
                self.estimated_decompress = cap;
                let mut buf = Vec::with_capacity(cap);

                if let Err(e) = decoder.read_to_end(&mut buf) {
                    return Some(Err(e.into()));
                }
                buf.into()
            }
        };

        self.bytes_after_decompress = decompressed.len();
        self.decompress_latency = start.elapsed();

        Some(Ok(decompressed))
    }

    // TODO: pass a buffer pool to avoid allocations
    pub fn decode<T: Decode + DeserializeOwned>(
        &mut self,
        body: &Bytes,
    ) -> Result<T, BuilderApiError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => T::from_ssz_bytes(body).map_err(BuilderApiError::SszDecode)?,
            Encoding::Json => serde_json::from_slice(body)?,
        };

        self.decode_latency = start.elapsed().saturating_sub(self.decompress_latency);
        self.record_metrics();

        Ok(payload)
    }

    pub fn decode_by_fork<T: ForkVersionDecode + DeserializeOwned>(
        &mut self,
        body: &Bytes,
        fork: ForkName,
    ) -> Result<T, BuilderApiError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => {
                T::from_ssz_bytes_by_fork(body, fork).map_err(BuilderApiError::SszDecode)?
            }
            Encoding::Json => serde_json::from_slice(body)?,
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
        MergeType, SignedBidSubmission, SignedBidSubmissionWithMergingData, TestRandomSeed,
    };
    use ssz::Encode;

    use super::*;

    #[test]
    fn test_get_builder_pubkey() {
        let expected = BlsPublicKeyBytes::from_hex("0x81f8ed149a60b16f4b22ba759f0a5420caa753768341bb41b27c15eb9b219afa5494f7d7b72d18c1a1b2904c66d2a30c").unwrap();

        let data_json =
            include_bytes!("../../../types/src/testdata/signed-bid-submission-fulu-2.json");
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

        let data_ssz = include_bytes!("../../../types/src/testdata/signed-bid-submission-fulu.ssz");
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
        let sub = SignedBidSubmission::test_random();
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
