use std::{
    io::Read,
    time::{Duration, Instant},
};

use alloy_primitives::Bytes;
use axum::response::{IntoResponse, Response};
use flate2::read::GzDecoder;
use helix_types::{
    BidAdjustmentData, BlockMergingData, Compression, DehydratedBidSubmission,
    DehydratedBidSubmissionFuluWithAdjustments, ForkName, ForkVersionDecode, MergeType,
    SignedBidSubmission, SignedBidSubmissionWithAdjustments, SignedBidSubmissionWithMergingData,
    Submission, Transaction, Transactions,
};
use http::{
    HeaderMap, HeaderValue, StatusCode,
    header::{ACCEPT, CONTENT_TYPE},
};
use serde::de::DeserializeOwned;
use ssz::Decode;
use ssz_derive::{Decode, Encode};
use ssz_types::VariableList;
use strum::{AsRefStr, EnumString};
use tracing::{debug, error, trace};
use tree_hash::{Hash256, TreeHash};
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
            let (mut sub, adjustment_data) = sub_with_adjustment.split();

            if !sub.is_dehydrated() {
                let expected_tx_root = sub.calculate_tx_root().unwrap();
                if let Some(current_tx_root) = sub.tx_root() {
                    if current_tx_root != expected_tx_root {
                        error!("dehydrated submission has invalid tx root");
                        return Err(DecoderError::PayloadDecode);
                    }
                } else {
                    sub.set_tx_root(expected_tx_root);
                    debug!(?expected_tx_root, "setting tx root for dehydrated submission");
                }
            }

            (sub, Some(adjustment_data))
        } else {
            let mut submission: DehydratedBidSubmission = self.decode_by_fork(body, self.fork_name)?;
            if !submission.is_dehydrated() {
                let expected_tx_root = submission.calculate_tx_root().unwrap();
                if let Some(current_tx_root) = submission.tx_root() {
                    if current_tx_root != expected_tx_root {
                        error!("dehydrated submission has invalid tx root");
                        return Err(DecoderError::PayloadDecode);
                    }
                } else {
                    submission.set_tx_root(expected_tx_root);
                    debug!(?expected_tx_root, "setting tx root for dehydrated submission");
                }
                
            }
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

pub fn tx_root_from_ssz(payload: &[u8]) -> Option<Hash256> {
    let start = Instant::now();
    let ep_start = u32::from_le_bytes(payload.get(236..240)?.try_into().ok()?) as usize;
    let tx_offset = u32::from_le_bytes(payload.get(ep_start + 504..ep_start + 508)?.try_into().ok()?) as usize;
    let withdrawals_offset = u32::from_le_bytes(payload.get(ep_start + 508..ep_start + 512)?.try_into().ok()?) as usize;
    let tx_list = payload.get(ep_start + tx_offset..ep_start + withdrawals_offset)?;
    let first_tx_offset = u32::from_le_bytes(tx_list.get(0..4)?.try_into().ok()?) as usize;
    let tx_count = first_tx_offset / 4;

    let mut txs = Vec::new();
    for i in 0..tx_count {
        let start = u32::from_le_bytes(tx_list.get(i*4..i*4+4)?.try_into().ok()?) as usize;
        let end = if i + 1 < tx_count {
            u32::from_le_bytes(tx_list.get((i+1)*4..(i+1)*4+4)?.try_into().ok()?) as usize
        } else {
            tx_list.len()
        };
        let tx_bytes = tx_list.get(start..end)?;
        if tx_bytes.len() == 8 {
            // dehydrated tx placeholder, can't compute root
            return None;
        }
        txs.push(Transaction(Bytes::copy_from_slice(tx_bytes)));
    }

    let transactions: Transactions = VariableList::new(txs).unwrap();
    let duration = start.elapsed();
    println!("tx_root_from_ssz took: {:?}", duration);
    Some(transactions.tree_hash_root())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

use alloy_eips::{eip4844::kzg_to_versioned_hash, eip7685::RequestsOrHash};
use alloy_primitives::{B256, hex::FromHex};
use alloy_rpc_types::engine::{CancunPayloadFields, ExecutionData, ExecutionPayload, ExecutionPayloadSidecar, PraguePayloadFields};
use helix_types::{BlobsBundle, MergeType};

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
    fn test_decode_tcp_submission_from_s3() {
        use helix_types::{Compression, ForkName, MergeType, Submission};

        let raw = include_bytes!(
            "../test_data/2026-05-13T10_36_09.464299763+00_00_9c111baa-7984-48c7-bd81-9f3d2a875f9c.bin"
        );

        let header_len = u16::from_le_bytes(raw[..2].try_into().unwrap()) as usize;
        let header = &raw[2..2 + header_len];
        let payload = &raw[2 + header_len..];
        let start = Instant::now();
        let root = tx_root_from_ssz(payload);
        let duration = start.elapsed();
        println!("tx_root_from_ssz took: {:?}", duration);
        assert_eq!(root, Some(B256::from_hex("0xee8513f442f7414172b9c543c93137148a3e309b3a0f31d3c63c7600ce53d677").unwrap()));

        let merge_type = MergeType::try_from(header[21]).unwrap();
        let flags = header[22];
        let encoding = match header[23] {
            0 => Encoding::Json,
            1 => Encoding::Ssz,
            b => panic!("unknown encoding byte {b}"),
        };
        let compression = match header[24] {
            0 => Compression::None,
            1 => Compression::Gzip,
            2 => Compression::Zstd,
            b => panic!("unknown compression byte {b}"),
        };

        let decoder_params = SubmissionDecoderParams {
            compression,
            encoding,
            is_dehydrated: flags & (1 << 0) != 0,
            merge_type,
            with_mergeable_data: merge_type.is_some(),
            with_adjustments: flags & (1 << 1) != 0,
            block_merging_dry_run: false,
            fork_name: ForkName::Fulu,
        };

        let mut buf = Vec::new();
        let (res, _, _) = SubmissionDecoder::new(&decoder_params).decode(payload, &mut buf).unwrap();
        match res {
            Submission::Full(sub) => assert_eq!(*sub.block_hash(), B256::from_hex("0xeda2e151114e1e7a9728d85c72ee4e3c7db21bd1d8dcdea88298371820ec3221").unwrap()),
            Submission::Dehydrated(sub) => {
                let s = match sub {
                    DehydratedBidSubmission::Fulu(subf) => subf,
                };

                let sidecar = BlobsBundle::with_capacity(s.blobs_bundle.commitments.len());

                let submission = SignedBidSubmission {
                    message: s.message,
                    execution_payload: Arc::new(s.execution_payload),
                    blobs_bundle: Arc::new(sidecar),
                    execution_requests: s.execution_requests,
                    signature: s.signature,
                };

                let v5: alloy_rpc_types::beacon::relay::SignedBidSubmissionV5 = submission.into();
                let execution_data = ExecutionData {
                    payload: ExecutionPayload::V3(v5.execution_payload),
                    sidecar: ExecutionPayloadSidecar::v4(
                        CancunPayloadFields {
                            parent_beacon_block_root: B256::from_hex("0x6841897da5752b1ab0ef580bb971df5adcdb1178c1f6796d95aad833400b7c60").unwrap(),
                            versioned_hashes: v5
                                .blobs_bundle
                                .commitments
                                .iter()
                                .map(|c| kzg_to_versioned_hash(c.as_slice()))
                                .collect(),
                        },
                        PraguePayloadFields {
                            requests: RequestsOrHash::Requests(v5.execution_requests.to_requests()),
                        },
                    ),
                };

                let header = execution_data.into_block_raw().unwrap().header;
                let block_hash = header.hash_slow();
                assert_eq!(block_hash, B256::from_hex("0xeda2e151114e1e7a9728d85c72ee4e3c7db21bd1d8dcdea88298371820ec3221").unwrap());
            },
        }
    }
}
