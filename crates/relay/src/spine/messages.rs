use flux_utils::{ArrayStr, DCacheRef};
use helix_common::SubmissionTrace;
use helix_tcp_types::Status;
use helix_types::BlsPublicKeyBytes;
use http::StatusCode;

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef},
};

#[derive(Debug, Clone, Copy)]
pub struct NewBidSubmission {
    pub dref: DCacheRef,
    pub payload_offset: usize,
    pub submission_ref: SubmissionRef,
    pub header: InternalBidSubmissionHeader,
    pub trace: SubmissionTrace,
    pub expected_pubkey: Option<BlsPublicKeyBytes>,
}

#[derive(Debug, Clone, Copy)]
pub struct SubmissionResultWithRef {
    pub sub_ref: SubmissionRef,
    pub tcp_status: Status,
    pub http_status: StatusCode,
    pub error_msg: ArrayStr<256>,
    pub should_report: bool,
}

impl SubmissionResultWithRef {
    pub fn new(sub_ref: SubmissionRef, result: Result<(), BuilderApiError>) -> Self {
        match result {
            Ok(()) => Self {
                sub_ref,
                tcp_status: Status::Okay,
                http_status: StatusCode::OK,
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
                    http_status: e.http_status(),
                    error_msg: ArrayStr::from_str_truncate(&e.to_string()),
                    should_report: e.should_report(),
                }
            }
        }
    }
}

// references position in SharedVector<BidSubmission>
#[derive(Debug, Clone, Copy)]
pub struct DecodedSubmission {
    pub ix: usize,
}
