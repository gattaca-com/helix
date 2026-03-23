use flux::timing::Nanos;
pub use tile::DecoderTile;

use crate::auctioneer::SubmissionData;

mod tile;

pub struct SubmissionDataWithSpan {
    pub submission_data: SubmissionData,
    pub span: tracing::Span,
    pub sent_at: Nanos,
}
