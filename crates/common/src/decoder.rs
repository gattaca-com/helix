use std::{
    io::Read,
    time::{Duration, Instant},
};

use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use flux_profiler::timed;
use helix_types::{
    BidAdjustmentData, BlockMergingData, Compression, DehydratedBidSubmission,
    DehydratedBidSubmissionFuluWithAdjustments, DehydratedBidSubmissionFuluWithMergingData,
    ForkName, ForkVersionDecode, MergeType, SignedBidSubmission,
    SignedBidSubmissionWithAdjustments, SignedBidSubmissionWithMergingData, Submission,
};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use ssz::Decode;
use ssz_derive::{Decode, Encode};
use strum::{AsRefStr, EnumString};
use tracing::{error, trace};
use zstd::{
    stream::read::Decoder as ZstdDecoder,
    zstd_safe::{CONTENTSIZE_ERROR, CONTENTSIZE_UNKNOWN, get_frame_content_size},
};

use crate::{
    api::builder_api::MAX_PAYLOAD_LENGTH,
    metrics::{
        BID_DECODING_LATENCY, BID_DECOMPRESS_SIZEHINT_REL_ERROR, DECOMPRESSION_LATENCY,
        SUBMISSION_BY_COMPRESSION, SUBMISSION_BY_ENCODING, SUBMISSION_COMPRESSED_BYTES,
        SUBMISSION_DECOMPRESSED_BYTES,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    #[error("json decode error: {0}")]
    JsonDecodeError(#[from] serde_json::Error),

    #[error("ssz decode error: {0:?}")]
    SszDecode(ssz::DecodeError),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("failed to decode payload")]
    PayloadDecode,
}

impl IntoResponse for DecoderError {
    fn into_response(self) -> Response {
        (&self).into_response()
    }
}

impl IntoResponse for &DecoderError {
    fn into_response(self) -> Response {
        (self.http_status(), self.to_string()).into_response()
    }
}

impl From<ssz::DecodeError> for DecoderError {
    fn from(e: ssz::DecodeError) -> Self {
        DecoderError::SszDecode(e)
    }
}

impl DecoderError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            DecoderError::JsonDecodeError(_) |
            DecoderError::SszDecode(_) |
            DecoderError::IOError(_) |
            DecoderError::PayloadDecode => StatusCode::BAD_REQUEST,
        }
    }
}

pub const HEADER_SUBMISSION_TYPE: &str = "x-submission-type";

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
#[derive(Clone, Copy, Debug)]
pub enum Encoding {
    Json = 0,
    Ssz = 1,
}

pub const HEADER_SSZ: &str = "application/octet-stream";
const HEADER_ACCEPT_SSZ: &str = "application/octet-stream;q=1.0,application/json;q=0.9";

impl ssz::Encode for Encoding {
    fn is_ssz_fixed_len() -> bool {
        true
    }
    fn ssz_fixed_len() -> usize {
        1
    }
    fn ssz_bytes_len(&self) -> usize {
        1
    }
    fn ssz_append(&self, buf: &mut Vec<u8>) {
        buf.push(*self as u8);
    }
}

impl ssz::Decode for Encoding {
    fn is_ssz_fixed_len() -> bool {
        true
    }
    fn ssz_fixed_len() -> usize {
        1
    }
    fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        match bytes {
            [0] => Ok(Encoding::Json),
            [1] => Ok(Encoding::Ssz),
            _ => Err(ssz::DecodeError::BytesInvalid(format!("invalid Encoding byte: {bytes:?}"))),
        }
    }
}

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

#[derive(Debug, Clone, Encode, Decode)]
pub struct SubmissionDecoderParams {
    pub compression: Compression,
    pub encoding: Encoding,
    pub merge_type: MergeType,
    pub is_dehydrated: bool,
    pub with_mergeable_data: bool,
    pub with_adjustments: bool,
    pub mark_all_txs_mergeable: bool,
    pub fork_name: ForkName,
}

#[derive(Debug)]
pub struct SubmissionDecoder {
    compression: Compression,
    encoding: Encoding,
    merge_type: MergeType,
    is_dehydrated: bool,
    with_mergeable_data: bool,
    with_adjustments: bool,
    mark_all_txs_mergeable: bool,
    fork_name: ForkName,

    bytes_before_decompress: usize,
    bytes_after_decompress: usize,
    estimated_decompress: usize,

    decompress_latency: Duration,
    decode_latency: Duration,
}

impl SubmissionDecoder {
    pub fn new(params: &SubmissionDecoderParams) -> Self {
        Self {
            compression: params.compression,
            encoding: params.encoding,
            merge_type: params.merge_type,
            is_dehydrated: params.is_dehydrated,
            with_mergeable_data: params.with_mergeable_data,
            with_adjustments: params.with_adjustments,
            mark_all_txs_mergeable: params.mark_all_txs_mergeable,
            fork_name: params.fork_name,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        }
    }

    #[timed]
    pub fn decompress(
        &mut self,
        payload: &[u8],
        buf: &mut Vec<u8>,
    ) -> Option<Result<(), DecoderError>> {
        let start = Instant::now();
        self.bytes_before_decompress = payload.len();

        match self.compression {
            Compression::None => return None,
            Compression::Gzip => {
                let cap = gzip_size_hint(payload).unwrap_or(payload.len() * 2);
                self.estimated_decompress = cap;
                buf.clear();
                buf.reserve(cap);
                let mut decoder = GzDecoder::new(payload).take(MAX_PAYLOAD_LENGTH as u64);
                if let Err(e) = decoder.read_to_end(buf) {
                    return Some(Err(e.into()));
                }
            }
            Compression::Zstd => {
                let cap = zstd_size_hint(payload).unwrap_or(payload.len() * 2);
                self.estimated_decompress = cap;
                buf.clear();
                buf.reserve(cap);
                let inner = match ZstdDecoder::new(payload) {
                    Ok(d) => d,
                    Err(e) => return Some(Err(e.into())),
                };
                let mut decoder = inner.take(MAX_PAYLOAD_LENGTH as u64);
                if let Err(e) = decoder.read_to_end(buf) {
                    return Some(Err(e.into()));
                }
            }
        }

        self.bytes_after_decompress = buf.len();
        self.decompress_latency = start.elapsed();

        Some(Ok(()))
    }

    #[timed]
    pub fn decode(
        &mut self,
        payload: &[u8],
        buf: &mut Vec<u8>,
    ) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), DecoderError>
    {
        let body: &[u8] = match self.decompress(payload, buf) {
            None => payload,
            Some(Ok(())) => buf,
            Some(Err(e)) => return Err(e),
        };

        if self.is_dehydrated {
            self.decode_dehydrated(body)
        } else if self.with_mergeable_data {
            self.decode_merge(body)
        } else {
            self.decode_default(body)
        }
    }

    #[timed]
    fn decode_dehydrated(
        &mut self,
        body: &[u8],
    ) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), DecoderError>
    {
        if self.merge_type == MergeType::Mergeable {
            let sub_with_merging: DehydratedBidSubmissionFuluWithMergingData =
                self.decode_by_fork(body, self.fork_name)?;
            let (submission, merging_data) = sub_with_merging.split();

            return Ok((Submission::Dehydrated(submission), Some(merging_data), None));
        }

        let (submission, bid_adjustment) = if self.with_adjustments {
            let sub_with_adjustment: DehydratedBidSubmissionFuluWithAdjustments =
                self.decode_by_fork(body, self.fork_name)?;
            let (sub, adjustment_data) = sub_with_adjustment.split();

            (sub, Some(adjustment_data))
        } else {
            let submission: DehydratedBidSubmission = self.decode_by_fork(body, self.fork_name)?;

            (submission, None)
        };

        let merging_data = match self.merge_type {
            MergeType::Mergeable => unreachable!("handled above"),
            MergeType::AppendOnly => {
                Some(BlockMergingData::append_only(submission.fee_recipient()))
            }
            MergeType::None => {
                if self.mark_all_txs_mergeable {
                    Some(BlockMergingData::allow_all(
                        submission.fee_recipient(),
                        submission.num_txs(),
                    ))
                } else {
                    None
                }
            }
            MergeType::Pause => None,
        };

        Ok((Submission::Dehydrated(submission), merging_data, bid_adjustment))
    }

    #[timed]
    fn decode_merge(
        &mut self,
        body: &[u8],
    ) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), DecoderError>
    {
        let sub_with_merging: SignedBidSubmissionWithMergingData = self._decode(body)?;
        let merging_data = match self.merge_type {
            MergeType::Mergeable => Some(sub_with_merging.merging_data),
            //Handle append-only by creating empty mergeable orders
            //this allows builder to switch between append-only and mergeable without changing
            // submission alternatively we could reject or ignore append-only here if the
            // submission is mergeable?
            MergeType::AppendOnly => Some(BlockMergingData {
                allow_appending: sub_with_merging.merging_data.allow_appending,
                builder_address: sub_with_merging.merging_data.builder_address,
                merge_orders: vec![],
            }),
            MergeType::None => Some(sub_with_merging.merging_data),
            MergeType::Pause => None,
        };
        Ok((Submission::Full(sub_with_merging.submission), merging_data, None))
    }

    #[timed]
    fn decode_default(
        &mut self,
        body: &[u8],
    ) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), DecoderError>
    {
        let (submission, bid_adjustment) = if self.with_adjustments {
            let sub_with_adjustment: SignedBidSubmissionWithAdjustments = self._decode(body)?;
            let (sub, adjustment_data) = sub_with_adjustment.split();

            (sub, Some(adjustment_data))
        } else {
            let submission: SignedBidSubmission = self._decode(body)?;

            (submission, None)
        };

        let merging_data = match self.merge_type {
            MergeType::Mergeable => {
                //Should this return an error instead?
                error!("mergeable dehydrated submissions are not supported");
                None
            }
            MergeType::AppendOnly => {
                Some(BlockMergingData::append_only(submission.fee_recipient()))
            }
            MergeType::None => {
                if self.mark_all_txs_mergeable {
                    Some(BlockMergingData::allow_all(
                        submission.fee_recipient(),
                        submission.num_txs(),
                    ))
                } else {
                    None
                }
            }
            MergeType::Pause => None,
        };
        Ok((Submission::Full(submission), merging_data, bid_adjustment))
    }

    // TODO: pass a buffer pool to avoid allocations
    fn _decode<T: Decode + DeserializeOwned>(&mut self, body: &[u8]) -> Result<T, DecoderError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => T::from_ssz_bytes(body).map_err(DecoderError::SszDecode)?,
            Encoding::Json => serde_json::from_slice(body)?,
        };

        self.decode_latency = start.elapsed().saturating_sub(self.decompress_latency);
        self.record_metrics();

        Ok(payload)
    }

    #[timed]
    pub fn decode_by_fork<T: ForkVersionDecode + DeserializeOwned>(
        &mut self,
        body: &[u8],
        fork: ForkName,
    ) -> Result<T, DecoderError> {
        let start = Instant::now();
        let payload: T = match self.encoding {
            Encoding::Ssz => {
                T::from_ssz_bytes_by_fork(body, fork).map_err(DecoderError::SszDecode)?
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
    use helix_types::{
        BidAdjData, BidAdjustmentDataV1, BlobsBundle, BundleOrder, DehydratedBidSubmission,
        DehydratedBidSubmissionFuluWithMergingData, MergeType, Order, TestRandom,
    };
    use ssz::Encode;

    use super::*;

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

    #[test]
    fn decode_dehydrated_mergeable_carries_real_merge_orders() {
        // `random_for_test` may produce an empty `merge_orders` (it's randomly sized); retry
        // until we get a non-empty one, since that's specifically what this test is proving
        // survives the decode (previously it was always dropped/forced-empty for dehydrated
        // submissions).
        let (body, expected_merging_data) = (0..100)
            .find_map(|_| {
                let submission =
                    DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());
                let (_, merging_data) = submission.clone().split();
                if merging_data.merge_orders.is_empty() {
                    return None;
                }
                Some((submission.as_ssz_bytes(), merging_data))
            })
            .expect("should produce a submission with non-empty merge_orders within 100 tries");

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::Mergeable,
            is_dehydrated: true,
            with_mergeable_data: false,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment_data) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Dehydrated(_)));
        assert!(bid_adjustment_data.is_none());
        assert_eq!(
            merging_data.expect("mergeable submission should carry merging data"),
            expected_merging_data
        );
    }

    #[test]
    fn decode_default_pause_never_carries_merge_data() {
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        // Random blobs bundles don't satisfy the Fulu cell-proof count on decode; irrelevant
        // to what this test is proving, so keep it empty to avoid unrelated flakiness.
        submission.blobs_bundle = BlobsBundle::default().into();
        let body = submission.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::Pause,
            is_dehydrated: false,
            with_mergeable_data: false,
            with_adjustments: false,
            // Even with this testing override on, Pause must still suppress merge data.
            mark_all_txs_mergeable: true,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment_data) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        assert!(bid_adjustment_data.is_none());
        assert!(merging_data.is_none());
    }

    #[test]
    fn decode_dehydrated_pause_never_carries_merge_data() {
        let with_merging =
            DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());
        let (submission, _) = with_merging.split();
        let DehydratedBidSubmission::Fulu(inner) = submission;
        let body = inner.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::Pause,
            is_dehydrated: true,
            with_mergeable_data: false,
            with_adjustments: false,
            // Even with this testing override on, Pause must still suppress merge data.
            mark_all_txs_mergeable: true,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment_data) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Dehydrated(_)));
        assert!(bid_adjustment_data.is_none());
        assert!(merging_data.is_none());
    }

    #[test]
    fn decode_default_with_adjustments_carries_adjustment_data() {
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        submission.blobs_bundle = BlobsBundle::default().into();
        let SignedBidSubmission {
            message,
            execution_payload,
            blobs_bundle,
            execution_requests,
            signature,
        } = submission;
        let bid_adjustment_data =
            BidAdjustmentData::V1(BidAdjustmentDataV1::Original(BidAdjData::default()));
        let with_adjustments = SignedBidSubmissionWithAdjustments {
            message,
            execution_payload,
            blobs_bundle,
            execution_requests,
            signature,
            bid_adjustment_data: bid_adjustment_data.clone(),
        };
        let body = with_adjustments.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::None,
            is_dehydrated: false,
            with_mergeable_data: false,
            with_adjustments: true,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        assert!(merging_data.is_none());
        assert_eq!(bid_adjustment.expect("adjustments should be carried"), bid_adjustment_data);
    }

    #[test]
    fn decode_merge_mergeable_carries_merging_data() {
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        submission.blobs_bundle = BlobsBundle::default().into();
        let merging_data = BlockMergingData::random_for_test(&mut rand::rng());
        let with_merging =
            SignedBidSubmissionWithMergingData { submission, merging_data: merging_data.clone() };
        let body = with_merging.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::Mergeable,
            is_dehydrated: false,
            with_mergeable_data: true,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, decoded_merging_data, bid_adjustment) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        assert!(bid_adjustment.is_none());
        assert_eq!(
            decoded_merging_data.expect("mergeable submission should carry merging data"),
            merging_data
        );
    }

    #[test]
    fn decode_merge_preserves_plain_bundle_order() {
        // Regression: `BundleOrder` briefly carried a stray `latest_only` field (added in one
        // commit, never removed, despite `Order::BundleV2`'s doc comment saying it was kept off
        // `BundleOrder` on purpose). That grew its SSZ fixed portion from 12 to 13 bytes, so
        // every builder — which only ever encodes the original 3-field, 12-byte shape — got its
        // entire merge submission rejected with `OffsetIntoFixedPortion(12)` the moment it
        // included so much as one plain (non-V2) bundle order.
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        submission.blobs_bundle = BlobsBundle::default().into();
        let merging_data = BlockMergingData {
            allow_appending: true,
            builder_address: submission.fee_recipient(),
            merge_orders: vec![Order::Bundle(BundleOrder {
                txs: vec![0, 1].into(),
                reverting_txs: vec![0].into(),
                dropping_txs: vec![].into(),
            })],
        };
        let with_merging =
            SignedBidSubmissionWithMergingData { submission, merging_data: merging_data.clone() };
        let body = with_merging.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::Mergeable,
            is_dehydrated: false,
            with_mergeable_data: true,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, decoded_merging_data, _) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        assert_eq!(
            decoded_merging_data.expect("mergeable submission should carry merging data"),
            merging_data
        );
    }

    #[test]
    fn decode_merge_append_only_clears_merge_orders() {
        // Retry until a non-empty `merge_orders` shows up, since that's what proves AppendOnly
        // actively clears them rather than happening to pass through an already-empty vec.
        let (body, expected_allow_appending, expected_builder_address) = (0..100)
            .find_map(|_| {
                let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
                submission.blobs_bundle = BlobsBundle::default().into();
                let merging_data = BlockMergingData::random_for_test(&mut rand::rng());
                if merging_data.merge_orders.is_empty() {
                    return None;
                }
                let allow_appending = merging_data.allow_appending;
                let builder_address = merging_data.builder_address;
                let with_merging = SignedBidSubmissionWithMergingData { submission, merging_data };
                Some((with_merging.as_ssz_bytes(), allow_appending, builder_address))
            })
            .expect("should produce a submission with non-empty merge_orders within 100 tries");

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::AppendOnly,
            is_dehydrated: false,
            with_mergeable_data: true,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, _) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        let merging_data = merging_data.expect("append-only should still carry merging data");
        assert!(merging_data.merge_orders.is_empty());
        assert_eq!(merging_data.allow_appending, expected_allow_appending);
        assert_eq!(merging_data.builder_address, expected_builder_address);
    }

    #[test]
    fn decode_dehydrated_with_adjustments_carries_adjustment_data() {
        // `DehydratedBidSubmissionFuluWithAdjustments` has no `TestRandom` impl and its fields
        // are private, so build one by swapping `merging_data` for `bid_adjustment_data` on a
        // JSON encoding of a random `..WithMergingData` (same field set otherwise).
        let with_merging =
            DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());
        let mut value = serde_json::to_value(&with_merging).expect("serialize");
        let obj = value.as_object_mut().expect("object");
        obj.remove("merging_data");
        let bid_adjustment_data =
            BidAdjustmentData::V1(BidAdjustmentDataV1::Original(BidAdjData::default()));
        obj.insert(
            "bid_adjustment_data".to_string(),
            serde_json::to_value(&bid_adjustment_data).expect("serialize adjustment data"),
        );
        let with_adjustments: DehydratedBidSubmissionFuluWithAdjustments =
            serde_json::from_value(value).expect("reconstruct with adjustments");
        let body = with_adjustments.as_ssz_bytes();

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Ssz,
            merge_type: MergeType::None,
            is_dehydrated: true,
            with_mergeable_data: false,
            with_adjustments: true,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Dehydrated(_)));
        assert!(merging_data.is_none());
        assert_eq!(bid_adjustment.expect("adjustments should be carried"), bid_adjustment_data);
    }

    #[test]
    fn decode_default_json_round_trip() {
        let mut submission = SignedBidSubmission::random_for_test(&mut rand::rng());
        submission.blobs_bundle = BlobsBundle::default().into();
        let body = serde_json::to_vec(&submission).expect("serialize");

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Json,
            merge_type: MergeType::None,
            is_dehydrated: false,
            with_mergeable_data: false,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Full(_)));
        assert!(merging_data.is_none());
        assert!(bid_adjustment.is_none());
    }

    #[test]
    fn decode_dehydrated_json_round_trip() {
        let with_merging =
            DehydratedBidSubmissionFuluWithMergingData::random_for_test(&mut rand::rng());
        let (submission, _) = with_merging.split();
        let body = serde_json::to_vec(&submission).expect("serialize");

        let params = SubmissionDecoderParams {
            compression: Compression::None,
            encoding: Encoding::Json,
            merge_type: MergeType::None,
            is_dehydrated: true,
            with_mergeable_data: false,
            with_adjustments: false,
            mark_all_txs_mergeable: false,
            fork_name: ForkName::Fulu,
        };
        let mut decoder = SubmissionDecoder::new(&params);
        let mut buf = Vec::new();
        let (decoded_submission, merging_data, bid_adjustment) =
            decoder.decode(&body, &mut buf).expect("decode should succeed");

        assert!(matches!(decoded_submission, Submission::Dehydrated(_)));
        assert!(merging_data.is_none());
        assert!(bid_adjustment.is_none());
    }
}
