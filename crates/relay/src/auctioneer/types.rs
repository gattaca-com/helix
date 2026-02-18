use std::{
    ops::{Deref, Range},
    sync::Arc,
    time::Instant,
};

use alloy_primitives::{B256, U256};
use helix_common::{
    GetPayloadTrace, SubmissionTrace,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        proposer_api::GetHeaderParams,
    },
    metrics::BID_CREATION_LATENCY,
};
use helix_tcp_types::{BidSubmissionFlags, BidSubmissionHeader};
use helix_types::{
    BidAdjustmentData, BlsPublicKeyBytes, BuilderBid, Compression, DehydratedBidSubmission,
    ExecutionPayload, ExecutionRequests, ForkName, GetPayloadResponse, MergeType,
    MergeableOrdersWithPref, PayloadAndBlobs, SignedBidSubmission, SignedBlindedBeaconBlock,
    SignedValidatorRegistration, Slot, SubmissionVersion, VersionedSignedProposal,
    mock_public_key_bytes,
};
use http::{
    HeaderMap, HeaderValue,
    header::{CONTENT_ENCODING, CONTENT_TYPE},
};
use rustc_hash::FxHashMap;
use serde::Serialize;
use ssz_derive::{Decode, Encode};
use tokio::sync::oneshot;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::{
        HEADER_API_KEY, HEADER_API_TOKEN, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_MERGE_TYPE,
        HEADER_SEQUENCE, HEADER_WITH_ADJUSTMENTS, builder::error::BuilderApiError,
        proposer::ProposerApiError,
    },
    auctioneer::{
        BlockMergeResult,
        decoder::{Encoding, SubmissionType},
        simulator::manager::SimulationResult,
    },
    gossip::BroadcastPayloadParams,
    housekeeper::PayloadAttributesUpdate,
};

pub type SubmissionResult = (Uuid, Result<(), BuilderApiError>);
pub type GetHeaderResult = Result<PayloadEntry, ProposerApiError>;
pub type GetPayloadResult = Result<GetPayloadResultData, ProposerApiError>;

pub struct InternalBidSubmissionHeader {
    pub id: Uuid,
    pub sequence_number: Option<u32>,
    pub merge_type: MergeType,
    pub flags: BidSubmissionFlags,
    pub encoding: Encoding,
    pub compression: Compression,
    pub api_key: Option<String>,
}

impl InternalBidSubmissionHeader {
    pub fn from_http_headers(request_id: Uuid, headers: http::header::HeaderMap) -> Self {
        let mut flags = BidSubmissionFlags::default();
        if matches!(headers.get(HEADER_WITH_ADJUSTMENTS), Some(header) if header == HeaderValue::from_static("true"))
        {
            flags.set(BidSubmissionFlags::WITH_ADJUSTMENTS, true);
        }
        if headers.get(HEADER_HYDRATE).is_some() {
            flags.set(BidSubmissionFlags::IS_DEHYDRATED, true);
        }

        let submission_type = SubmissionType::from_headers(&headers);
        if submission_type.is_some_and(|sub_type| sub_type == SubmissionType::Dehydrated) {
            flags.set(BidSubmissionFlags::IS_DEHYDRATED, true);
        }

        let sequence_number = headers
            .get(HEADER_SEQUENCE)
            .and_then(|seq| seq.to_str().ok())
            .and_then(|seq| seq.parse::<u32>().ok());

        const GZIP_HEADER: HeaderValue = HeaderValue::from_static("gzip");
        const ZSTD_HEADER: HeaderValue = HeaderValue::from_static("zstd");

        let compression = match headers.get(CONTENT_ENCODING) {
            Some(header) if header == GZIP_HEADER => Compression::Gzip,
            Some(header) if header == ZSTD_HEADER => Compression::Zstd,
            _ => Compression::None,
        };

        const SSZ_HEADER: HeaderValue = HeaderValue::from_static("application/octet-stream");

        let encoding = match headers.get(CONTENT_TYPE) {
            Some(header) if header == SSZ_HEADER => Encoding::Ssz,
            _ => Encoding::Json,
        };

        let merge_type = Self::merge_type_from_headers(&headers, submission_type);

        let api_key = headers
            .get(HEADER_API_KEY)
            .or(headers.get(HEADER_API_TOKEN))
            .and_then(|key| key.to_str().map(|key| key.to_owned()).ok());

        Self { id: request_id, sequence_number, merge_type, flags, encoding, compression, api_key }
    }

    fn merge_type_from_headers(
        header_map: &HeaderMap,
        sub_type: Option<SubmissionType>,
    ) -> MergeType {
        match header_map.get(HEADER_MERGE_TYPE) {
            None => {
                if sub_type.is_some_and(|sub_type| sub_type == SubmissionType::Merge) ||
                    matches!(header_map.get(HEADER_IS_MERGEABLE), Some(header) if header == HeaderValue::from_static("true"))
                {
                    MergeType::Mergeable
                } else {
                    MergeType::None
                }
            }
            Some(merge_type) => {
                merge_type.to_str().ok().and_then(|t| t.parse().ok()).unwrap_or_default()
            }
        }
    }

    pub fn from_tcp_header(request_id: Uuid, header: BidSubmissionHeader) -> Self {
        Self {
            id: request_id,
            sequence_number: Some(header.sequence_number),
            merge_type: header.merge_type,
            flags: header.flags,
            encoding: Encoding::Ssz,
            compression: header.compression(),
            api_key: None,
        }
    }
}

pub enum SubmissionResultSender<T> {
    OneShot(oneshot::Sender<T>),
    Shared(crossbeam_channel::Sender<T>),
}

impl<T> SubmissionResultSender<T> {
    pub fn try_send(self, data: T) {
        match self {
            Self::OneShot(tx) => {
                let _ = tx.send(data);
            }
            Self::Shared(tx) => {
                if let Err(e) = tx.try_send(data) {
                    tracing::error!(err=%e, "failed to send submission result to worker");
                }
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            Self::OneShot(tx) => tx.is_closed(),
            Self::Shared(_) => false,
        }
    }
}

pub struct GetPayloadResultData {
    pub to_proposer: GetPayloadResponse,
    pub to_publish: VersionedSignedProposal,
    pub trace: GetPayloadTrace,
    pub fork: ForkName,
    pub bid: PayloadBidData,
}

#[derive(Clone, Debug)]
pub struct SubmissionData {
    pub submission_id: Uuid,
    pub submission: Submission,
    pub merging_data: Option<MergeableOrdersWithPref>,
    pub bid_adjustment_data: Option<BidAdjustmentData>,
    pub version: SubmissionVersion,
    pub withdrawals_root: B256,
    pub trace: SubmissionTrace,
}

impl Deref for SubmissionData {
    type Target = Submission;

    fn deref(&self) -> &Self::Target {
        &self.submission
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Submission {
    // received after sigverify
    Full(SignedBidSubmission),
    // need to validate do the validate_payload_ssz_lengths
    Dehydrated(DehydratedBidSubmission),
}

impl Submission {
    pub fn bid_slot(&self) -> u64 {
        match self {
            Submission::Full(s) => s.slot().as_u64(),
            Submission::Dehydrated(s) => s.slot(),
        }
    }

    pub fn builder_pubkey(&self) -> &BlsPublicKeyBytes {
        match self {
            Submission::Full(s) => &s.message().builder_pubkey,
            Submission::Dehydrated(s) => s.builder_pubkey(),
        }
    }

    pub fn block_hash(&self) -> &B256 {
        match self {
            Submission::Full(s) => &s.message().block_hash,
            Submission::Dehydrated(s) => s.block_hash(),
        }
    }

    pub fn withdrawal_root(&self) -> B256 {
        match self {
            Submission::Full(s) => s.withdrawals_root(),
            Submission::Dehydrated(s) => s.withdrawal_root(),
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match self {
            Submission::Full(s) => s.parent_hash(),
            Submission::Dehydrated(s) => s.parent_hash(),
        }
    }
}

#[derive(Clone)]
pub struct GossipPayload {
    pub payload_and_blobs: PayloadAndBlobs,
    pub bid_data: PayloadBidData,
}

#[derive(Clone)]
pub struct SubmissionPayload {
    pub signed_bid_submission: SignedBidSubmission,
    pub withdrawals_root: B256,
    pub tx_root: Option<B256>,
    pub bid_adjustment_data: Option<BidAdjustmentData>,
    pub is_adjusted: bool,
    pub submission_version: SubmissionVersion,
    pub submission_trace: SubmissionTrace,
    pub parent_beacon_block_root: Option<B256>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum PayloadEntry {
    Submission(SubmissionPayload),
    Gossip(GossipPayload),
}

impl PayloadEntry {
    pub fn new_submission(
        signed_bid_submission: SignedBidSubmission,
        withdrawals_root: B256,
        tx_root: Option<B256>,
        bid_adjustment_data: Option<BidAdjustmentData>,
        submission_version: SubmissionVersion,
        submission_trace: SubmissionTrace,
        parent_beacon_block_root: Option<B256>,
    ) -> Self {
        Self::Submission(SubmissionPayload {
            signed_bid_submission,
            withdrawals_root,
            tx_root,
            bid_adjustment_data,
            is_adjusted: false,
            submission_version,
            submission_trace,
            parent_beacon_block_root,
        })
    }

    pub fn new_gossip(payload_and_blobs: PayloadAndBlobs, bid_data: PayloadBidData) -> Self {
        Self::Gossip(GossipPayload { payload_and_blobs, bid_data })
    }

    pub fn is_adjusted(&self) -> bool {
        matches!(self, Self::Submission(s) if s.is_adjusted)
    }

    pub fn is_adjustable(&self) -> bool {
        matches!(self, Self::Submission(s) if s.bid_adjustment_data.is_some())
    }

    pub fn value(&self) -> &U256 {
        match &self {
            Self::Submission(s) => s.signed_bid_submission.value(),
            Self::Gossip(s) => &s.bid_data.value,
        }
    }

    pub fn payload_and_blobs(&self) -> PayloadAndBlobs {
        match &self {
            Self::Submission(s) => s.signed_bid_submission.payload_and_blobs(),
            Self::Gossip(s) => s.payload_and_blobs.clone(),
        }
    }

    pub fn parent_hash(&self) -> &B256 {
        match &self {
            Self::Submission(s) => &s.signed_bid_submission.execution_payload_ref().parent_hash,
            Self::Gossip(s) => &s.payload_and_blobs.execution_payload.parent_hash,
        }
    }

    pub fn bid_data_ref(&self) -> PayloadBidDataRef<'_> {
        match &self {
            Self::Submission(s) => PayloadBidDataRef {
                withdrawals_root: &s.withdrawals_root,
                tx_root: &s.tx_root,
                execution_requests: s.signed_bid_submission.execution_requests_ref(),
                value: s.signed_bid_submission.value(),
                builder_pubkey: s.signed_bid_submission.builder_public_key(),
            },
            Self::Gossip(s) => PayloadBidDataRef::from(&s.bid_data),
        }
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        match self {
            Self::Submission(bid) => bid.signed_bid_submission.execution_payload_ref(),
            Self::Gossip(bid) => &bid.payload_and_blobs.execution_payload,
        }
    }

    pub fn execution_payload_make_mut(&mut self) -> &mut ExecutionPayload {
        match self {
            Self::Submission(bid) => bid.signed_bid_submission.execution_payload_make_mut(),
            Self::Gossip(bid) => Arc::make_mut(&mut bid.payload_and_blobs.execution_payload),
        }
    }

    pub fn block_hash(&self) -> &B256 {
        &self.execution_payload().block_hash
    }

    /// This may be slow because of the tx root
    pub fn into_builder_bid_slow(self) -> BuilderBid {
        let start = Instant::now();

        let (withdrawals_root, tx_root, execution_requests, commitments) = match &self {
            Self::Submission(s) => (
                Some(s.withdrawals_root),
                s.tx_root,
                s.signed_bid_submission.execution_requests_ref().clone(),
                s.signed_bid_submission.blobs_bundle().commitments().to_owned(),
            ),
            Self::Gossip(s) => (
                None,
                None,
                s.bid_data.execution_requests.clone(),
                s.payload_and_blobs.blobs_bundle.commitments().to_owned(),
            ),
        };

        let header = self.execution_payload().to_header(withdrawals_root, tx_root);

        let bid = BuilderBid {
            header,
            blob_kzg_commitments: commitments,
            value: *self.value(),
            execution_requests,
            pubkey: mock_public_key_bytes(),
        };

        BID_CREATION_LATENCY.observe(start.elapsed().as_micros() as f64);
        debug!("creating builder bid took {:?}", start.elapsed());

        bid
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PayloadBidData {
    pub withdrawals_root: B256,
    pub tx_root: Option<B256>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub value: U256,
    pub builder_pubkey: BlsPublicKeyBytes,
}

#[derive(Clone, PartialEq, Debug, Encode, Serialize)]
pub struct PayloadBidDataRef<'a> {
    pub withdrawals_root: &'a B256,
    pub tx_root: &'a Option<B256>,
    pub execution_requests: &'a Arc<ExecutionRequests>,
    pub value: &'a U256,
    pub builder_pubkey: &'a BlsPublicKeyBytes,
}

impl<'a> From<&'a PayloadBidData> for PayloadBidDataRef<'a> {
    fn from(bid_data: &'a PayloadBidData) -> Self {
        Self {
            withdrawals_root: &bid_data.withdrawals_root,
            tx_root: &bid_data.tx_root,
            execution_requests: &bid_data.execution_requests,
            value: &bid_data.value,
            builder_pubkey: &bid_data.builder_pubkey,
        }
    }
}

impl PayloadBidDataRef<'_> {
    pub fn to_owned(&self) -> PayloadBidData {
        let withdrawals_root = *self.withdrawals_root;
        let tx_root = *self.tx_root;
        let execution_requests = self.execution_requests.clone();
        let value = *self.value;
        let builder_pubkey = *self.builder_pubkey;
        PayloadBidData { withdrawals_root, tx_root, execution_requests, value, builder_pubkey }
    }
}

pub enum SubWorkerJob {
    BlockSubmission {
        header: InternalBidSubmissionHeader,
        body: bytes::Bytes,
        trace: SubmissionTrace, // TODO: replace this with better tracing
        res_tx: SubmissionResultSender<SubmissionResult>,
        span: tracing::Span,
        sent_at: Instant,
        expected_pubkey: Option<BlsPublicKeyBytes>,
    },

    GetPayload {
        blinded_block: Box<SignedBlindedBeaconBlock>,
        proposer_pubkey: BlsPublicKeyBytes,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
        span: tracing::Span,
    },
}

pub struct RegWorkerJob {
    pub regs: Arc<Vec<SignedValidatorRegistration>>,
    pub range: Range<usize>,
    /// (Index in regs, has passed verification)
    pub res_tx: oneshot::Sender<Vec<(usize, bool)>>,
}

#[derive(Clone)]
pub struct SlotData {
    /// Head slot + 1, builders are bidding to build this slot
    pub bid_slot: Slot,
    /// Data about the validator registration
    pub registration_data: BuilderGetValidatorsResponseEntry,
    /// Parent hash -> payload attributes for the incoming blocks
    pub payload_attributes_map: FxHashMap<B256, PayloadAttributesUpdate>,
    /// Current fork
    pub current_fork: ForkName,
    /// Inclusion list
    pub il: Option<InclusionListWithMetadata>,
}

// cpu "intensive" stuff is:
// parsing headers, decode, hydration, sig verify, withdrawals and tx root, mergeable orders,
// get payload stuff, re-signing get header, encoding requests to JSON

pub struct PendingPayload {
    pub block_hash: B256,
    pub blinded: SignedBlindedBeaconBlock,
    pub trace: GetPayloadTrace,
    pub res_tx: oneshot::Sender<GetPayloadResult>,
}

impl SlotData {
    pub fn proposer_pubkey(&self) -> &BlsPublicKeyBytes {
        &self.registration_data.entry.registration.message.pubkey
    }
}

#[allow(clippy::large_enum_variant)]
pub enum Event {
    // Assume this data is already validate, ie valid this bid_slot
    SlotData {
        /// Head slot + 1, builders are bidding to build this slot
        bid_slot: Slot,
        /// Data about the validator registration
        registration_data: Option<BuilderGetValidatorsResponseEntry>,
        /// Payload attributes for the incoming blocks
        payload_attributes: Option<PayloadAttributesUpdate>,
        /// Inclusion list
        il: Option<InclusionListWithMetadata>,
    },
    Submission {
        submission_data: SubmissionData,
        res_tx: SubmissionResultSender<SubmissionResult>,
        span: tracing::Span,
        sent_at: Instant,
    },
    /// Assume already some validation (so we don't have to wait here)
    /// timing games already done
    GetHeader {
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
        span: tracing::Span,
    },
    // Receive multiple of these potentially, assume some light validation
    GetPayload {
        block_hash: B256,
        blinded: Box<SignedBlindedBeaconBlock>,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
        span: tracing::Span,
    },
    GossipPayload(BroadcastPayloadParams),
    SimResult(SimulationResult),
    SimulatorSync {
        id: usize,
        is_synced: bool,
    },
    MergeResult(BlockMergeResult),
}

impl Event {
    pub fn as_str(&self) -> &'static str {
        match &self {
            Event::SlotData { .. } => "SlotData",
            Event::Submission { .. } => "Submission",
            Event::GetHeader { .. } => "GetHeader",
            Event::GetPayload { .. } => "GetPayload",
            Event::GossipPayload(_) => "GossipPayload",
            Event::SimResult(_) => "SimResult",
            Event::SimulatorSync { .. } => "SimulatorSync",
            Event::MergeResult(_) => "MergeResult",
        }
    }
}

impl SubWorkerJob {
    pub fn as_str(&self) -> &'static str {
        match &self {
            SubWorkerJob::BlockSubmission { .. } => "BlockSubmission",
            SubWorkerJob::GetPayload { .. } => "GetPayload",
        }
    }
}
