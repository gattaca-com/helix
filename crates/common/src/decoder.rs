use std::{
    io::Read,
    time::{Duration, Instant},
};

use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use helix_types::{
    BidAdjustmentData, BlockMergingData, Compression, DehydratedBidSubmission,
    DehydratedBidSubmissionFuluWithAdjustments, ForkName, ForkVersionDecode, MergeType,
    SignedBidSubmission, SignedBidSubmissionWithAdjustments, SignedBidSubmissionWithMergingData,
    Submission,
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
    pub block_merging_dry_run: bool,
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
    block_merging_dry_run: bool,
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
            block_merging_dry_run: params.block_merging_dry_run,
            fork_name: params.fork_name,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        }
    }

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

    fn decode_dehydrated(
        &mut self,
        body: &[u8],
    ) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), DecoderError>
    {
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
            MergeType::Mergeable => {
                //Should this return an error instead?
                error!("mergeable dehydrated submissions are not supported");
                None
            }
            MergeType::AppendOnly => {
                Some(BlockMergingData::append_only(submission.fee_recipient()))
            }
            MergeType::None => {
                if self.block_merging_dry_run {
                    Some(BlockMergingData::append_only(submission.fee_recipient()))
                } else {
                    None
                }
            }
        };

        Ok((Submission::Dehydrated(submission), merging_data, bid_adjustment))
    }

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
        };
        Ok((Submission::Full(sub_with_merging.submission), merging_data, None))
    }

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
                if self.block_merging_dry_run {
                    Some(BlockMergingData::allow_all(
                        submission.fee_recipient(),
                        submission.num_txs(),
                    ))
                } else {
                    None
                }
            }
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
    use helix_types::MergeType;

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
}
