use flux::timing::Nanos;
use flux_utils::DCacheRef;
pub use tile::DecoderTile;

use crate::auctioneer::SubmissionData;

mod tile;

pub struct SubmissionDataWithSpan {
    pub submission_data: SubmissionData,
    pub span: tracing::Span,
    pub sent_at: Nanos,
    pub original_data_ref: DCacheRef,
}
