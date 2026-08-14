use alloy_primitives::B256;
use flux_utils::ArrayStr;
use helix_common::SubmissionTrace;
// Re-export as also used as spine message.
pub use helix_common::api::builder_api::TopBidUpdate;
use helix_tcp_types::Status;
use helix_types::BlsPublicKeyBytes;
use http::StatusCode;

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef},
};

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct NewBidSubmission {
    pub payload_offset: usize,
    pub submission_ref: SubmissionRef,
    pub header: InternalBidSubmissionHeader,
    pub trace: SubmissionTrace,
    // `Option<BlsPublicKeyBytes>` would make this type FFI-unsafe for the
    // spine's extern "C" queue functions (no niche to encode `None`), so
    // absence is tracked out of band here.
    pub expected_pubkey: BlsPublicKeyBytes,
    pub has_expected_pubkey: bool,
    // always populated by both construction sites; a plain index rather
    // than `Option<usize>` for the same FFI-safety reason.
    pub http_submission_ix: usize,
}

impl NewBidSubmission {
    pub fn expected_pubkey(&self) -> Option<&BlsPublicKeyBytes> {
        self.has_expected_pubkey.then_some(&self.expected_pubkey)
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SubmissionResultWithRef {
    pub sub_ref: SubmissionRef,
    pub tcp_status: Status,
    // `http::StatusCode` has no repr attribute, so it can't cross the spine's
    // extern "C" queues; stored as the raw code instead.
    pub http_status_code: u16,
    pub error_msg: ArrayStr<256>,
    pub should_report: bool,
}

impl SubmissionResultWithRef {
    pub fn new(sub_ref: SubmissionRef, result: Result<(), BuilderApiError>) -> Self {
        match result {
            Ok(()) => Self {
                sub_ref,
                tcp_status: Status::Okay,
                http_status_code: StatusCode::OK.as_u16(),
                error_msg: ArrayStr::default(),
                should_report: false,
            },
            Err(ref e) => {
                let tcp_status = match e {
                    BuilderApiError::DatabaseError(_) | BuilderApiError::InternalError => {
                        Status::InternalError
                    }
                    _ => Status::InvalidRequest,
                };
                Self {
                    sub_ref,
                    tcp_status,
                    http_status_code: e.http_status().as_u16(),
                    error_msg: ArrayStr::from_str_truncate(&e.to_string()),
                    should_report: e.should_report(),
                }
            }
        }
    }

    pub fn http_status(&self) -> StatusCode {
        StatusCode::from_u16(self.http_status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

// references position in SharedVector<BidSubmission>
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct DecodedSubmission {
    pub ix: usize,
}

/// Auctioneer → SimulatorTile: spine signal for a new sim/merge request or slot transition.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ToSimMsg {
    pub kind: ToSimKind,
    /// Index into `SharedVector<SimInboundPayload>` (unused for `NewSlot`).
    pub ix: usize,
    /// Slot number; only meaningful for `NewSlot`.
    pub bid_slot: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum ToSimKind {
    /// SimRequest or MergeRequest stored at `ix`.
    Request,
    NewSlot,
}

/// SimulatorTile → Auctioneer: index into `SharedVector<SimOutboundPayload>`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct FromSimMsg {
    pub ix: usize,
}

/// BlockMergingTile → Auctioneer: index into `SharedVector<BlockMergeResponse>`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MergedBlockMsg {
    pub ix: usize,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub enum BidEvent {
    Live,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct BidUpdate {
    pub block_hash: B256,
    pub event: BidEvent,
}

/// HousekeeperTile → all consumers: index into `SharedVector<SlotUpdate>`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SlotMsg {
    pub ix: usize,
}
