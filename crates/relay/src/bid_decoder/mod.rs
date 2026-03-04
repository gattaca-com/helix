use std::time::Instant;

pub use decoder::{Encoding, HEADER_SSZ, SubmissionType};
pub use tile::DecoderTile;

use crate::auctioneer::SubmissionData;

mod decoder;
mod tile;

pub struct SubmissionDataWithSpan {
    pub submission_data: SubmissionData,
    pub span: tracing::Span,
    pub sent_at: Instant,
}
