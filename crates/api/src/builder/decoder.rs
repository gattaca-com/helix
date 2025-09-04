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
use helix_types::{SignedBidSubmission, SignedBidSubmissionWithMergingData};
use http::{
    header::{CONTENT_ENCODING, CONTENT_TYPE},
    HeaderMap, HeaderValue,
};
use tracing::trace;
use zstd::{
    stream::read::Decoder as ZstdDecoder,
    zstd_safe::{get_frame_content_size, CONTENTSIZE_ERROR, CONTENTSIZE_UNKNOWN},
};

use crate::builder::{api::MAX_PAYLOAD_LENGTH, error::BuilderApiError};

const HEADER_IS_MERGEABLE: &str = "x-mergeable";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Json,
    Ssz,
}

#[derive(Debug)]
pub struct SubmissionDecoder {
    compression: Compression,
    encoding: Encoding,
    has_mergeable_data: bool,

    bytes_before_decompress: usize,
    bytes_after_decompress: usize,
    estimated_decompress: usize,

    decompress_latency: Duration,
    decode_latency: Duration,
}

impl SubmissionDecoder {
    pub fn from_headers(header_map: &HeaderMap) -> Self {
        const GZIP_HEADER: HeaderValue = HeaderValue::from_static("gzip");
        const ZSTD_HEADER: HeaderValue = HeaderValue::from_static("zstd");

        let compression = match header_map.get(CONTENT_ENCODING) {
            Some(header) if header == GZIP_HEADER => Compression::Gzip,
            Some(header) if header == ZSTD_HEADER => Compression::Zstd,
            _ => Compression::None,
        };

        const SSZ_HEADER: HeaderValue = HeaderValue::from_static("application/octet-stream");

        let encoding = match header_map.get(CONTENT_TYPE) {
            Some(header) if header == SSZ_HEADER => Encoding::Ssz,
            _ => Encoding::Json,
        };

        const TRUE_HEADER: HeaderValue = HeaderValue::from_static("true");

        let has_mergeable_data =
            matches!(header_map.get(HEADER_IS_MERGEABLE), Some(header) if header == TRUE_HEADER);

        Self {
            compression,
            encoding,
            has_mergeable_data,
            bytes_before_decompress: 0,
            bytes_after_decompress: 0,
            estimated_decompress: 0,
            decompress_latency: Default::default(),
            decode_latency: Default::default(),
        }
    }

    // TODO: pass a buffer pool to avoid allocations
    pub fn decode(
        mut self,
        body: Bytes,
    ) -> Result<SignedBidSubmissionWithMergingData, BuilderApiError> {
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

        trace!(
            size_compressed = self.bytes_before_decompress,
            size_uncompressed = self.bytes_after_decompress,
            compression =? self.compression,
            "decompressed payload"
        );

        let payload: SignedBidSubmissionWithMergingData = if self.has_mergeable_data {
            decode_submission(self.encoding, &decompressed)?
        } else {
            let submission: SignedBidSubmission = decode_submission(self.encoding, &decompressed)?;
            SignedBidSubmissionWithMergingData { submission, merging_data: Default::default() }
        };

        self.decode_latency = start.elapsed().saturating_sub(self.decompress_latency);
        self.record_metrics();

        Ok(payload)
    }

    fn record_metrics(&self) {
        let compression_label = match self.compression {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
        };
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
        // Record encoding type
        let encoding_label = match self.encoding {
            Encoding::Json => "json",
            Encoding::Ssz => "ssz",
        };
        SUBMISSION_BY_ENCODING.with_label_values(&[encoding_label]).inc();
        BID_DECODING_LATENCY
            .with_label_values(&[encoding_label])
            .observe(self.decode_latency.as_micros() as f64);
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

fn decode_submission<'a, T>(encoding: Encoding, bytes: &'a Bytes) -> Result<T, BuilderApiError>
where
    T: ssz::Decode + serde::Deserialize<'a>,
{
    let payload = match encoding {
        Encoding::Ssz => T::from_ssz_bytes(bytes)
            .map_err(|err| BuilderApiError::SszDeserializeError(format!("{err:?}")))?,
        Encoding::Json => serde_json::from_slice(bytes)?,
    };
    Ok(payload)
}
