use flux::type_hash_derive::type_hash_lock;
use flux_utils::ArrayStr;
use flux_versioned_types::versioned_struct;
use helix_common::SubmissionTrace;
use helix_tcp_types::Status;
pub use helix_telemetry::{
    BidUpdate, DecodedSubmission, MergedBlockMsg, SimFinished, SimStarted, SimTxIncluded,
    SimUpdate, SlotMsg, TopBidUpdate,
};
use helix_types::BlsPublicKeyBytes;
use http::StatusCode;
use uuid::Uuid;

use crate::{
    api::builder::error::BuilderApiError,
    auctioneer::{InternalBidSubmissionHeader, SubmissionRef},
};

versioned_struct!(NewBidSubmission =>
    #[derive(Default)]
    #[type_hash_lock(hash = 17253066427936622228)]
    NewBidSubmissionV1 {
        pub payload_offset: usize,
        pub submission_ref: SubmissionRef,
        pub header: InternalBidSubmissionHeader,
        pub trace: SubmissionTrace,
        // `Option<BlsPublicKeyBytes>` would make this type FFI-unsafe for the
        // spine's extern "C" queue functions (no niche to encode `None`), so
        // absence is tracked out of band here.
        pub expected_pubkey: BlsPublicKeyBytes,
        pub has_expected_pubkey: bool,
        pub _pad: [u8; 7],
    }
);

versioned_struct!(NewTcpBidSubmission =>
    #[type_hash_lock(hash = 2065262409252429600)]
    NewTcpBidSubmissionV1 {
        pub inner: NewBidSubmission,
    }
);

impl NewBidSubmission {
    pub fn expected_pubkey(&self) -> Option<&BlsPublicKeyBytes> {
        self.has_expected_pubkey.then_some(&self.expected_pubkey)
    }
}

versioned_struct!(SubmissionResultWithRef =>
    #[derive(Default)]
    #[type_hash_lock(hash = 157972463081440812)]
    SubmissionResultWithRefV1 {
        pub sub_ref: SubmissionRef,
        pub submission_id: Uuid,
        pub tcp_status: Status,
        _pad0: [u8; 1],
        // `http::StatusCode` has no repr attribute, so it can't cross the spine's
        // extern "C" queues; stored as the raw code instead.
        pub http_status_code: u16,
        _pad1: [u8; 4],
        pub error_msg: ArrayStr<256>,
        pub should_report: bool,
        _pad2: [u8; 7],
    }
);

impl SubmissionResultWithRef {
    pub fn new(
        submission_id: Uuid,
        sub_ref: SubmissionRef,
        result: Result<(), BuilderApiError>,
    ) -> Self {
        match result {
            Ok(()) => Self {
                submission_id,
                sub_ref,
                tcp_status: Status::Okay,
                http_status_code: StatusCode::OK.as_u16(),
                error_msg: ArrayStr::default(),
                should_report: false,
                ..Default::default()
            },
            Err(ref e) => {
                let tcp_status = match e {
                    BuilderApiError::DatabaseError(_) | BuilderApiError::InternalError => {
                        Status::InternalError
                    }
                    _ => Status::InvalidRequest,
                };
                Self {
                    submission_id,
                    sub_ref,
                    tcp_status,
                    http_status_code: e.http_status().as_u16(),
                    error_msg: ArrayStr::from_str_truncate(&e.to_string()),
                    should_report: e.should_report(),
                    ..Default::default()
                }
            }
        }
    }

    pub fn http_status(&self) -> StatusCode {
        StatusCode::from_u16(self.http_status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}
