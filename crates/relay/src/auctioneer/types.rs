use std::{ops::Range, sync::Arc, time::Instant};

use alloy_primitives::{B256, U256};
use helix_common::{
    GetPayloadTrace, SubmissionTrace,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        proposer_api::GetHeaderParams,
    },
    metrics::BID_CREATION_LATENCY,
};
use helix_types::{
    BlockMergingPreferences, BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission,
    ExecutionPayload, ExecutionRequests, ForkName, GetPayloadResponse, PayloadAndBlobs,
    SignedBidSubmission, SignedBlindedBeaconBlock, SignedValidatorRegistration, Slot,
    SubmissionVersion, VersionedSignedProposal, mock_public_key_bytes,
};
use rustc_hash::FxHashMap;
use tokio::sync::oneshot;
use tracing::debug;

use crate::{
    api::{builder::error::BuilderApiError, proposer::ProposerApiError},
    auctioneer::{BlockMergeRequest, simulator::manager::SimulationResult},
    gossip::BroadcastPayloadParams,
    housekeeper::PayloadAttributesUpdate,
};

pub type SubmissionResult = Result<(), BuilderApiError>;
pub type GetHeaderResult = Result<PayloadHeaderData, ProposerApiError>;
pub type GetPayloadResult = Result<GetPayloadResultData, ProposerApiError>;
pub type BestMergeablePayload = Option<PayloadHeaderData>;

pub struct GetPayloadResultData {
    pub to_proposer: GetPayloadResponse,
    pub to_publish: VersionedSignedProposal,
    pub trace: GetPayloadTrace,
    pub fork: ForkName,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
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

/// From a SignedBidSubmission, keep only the fields needed to serve get_header and get_payload,
/// some fields are optional because payloads also arrive via gossip and we only gossip
/// PayloadAndBlobs
pub struct PayloadEntry {
    /// Either from a submission or via gossip
    pub payload_and_blobs: Arc<PayloadAndBlobs>,
    /// Some only if we processed the submission locally
    pub bid_data: Option<PayloadBidData>,
}

impl PayloadEntry {
    pub fn new_submission(
        signed_bid_submission: SignedBidSubmission,
        withdrawals_root: B256,
        tx_root: Option<B256>,
    ) -> Self {
        Self {
            payload_and_blobs: signed_bid_submission.payload_and_blobs_ref().to_owned().into(),
            bid_data: Some(PayloadBidData {
                withdrawals_root,
                tx_root,
                execution_requests: signed_bid_submission.execution_requests().clone(),
                value: signed_bid_submission.value(),
            }),
        }
    }

    pub fn new_gossip(data: BroadcastPayloadParams) -> Self {
        Self { payload_and_blobs: data.execution_payload, bid_data: None }
    }

    pub fn to_header_data(&self) -> Option<PayloadHeaderData> {
        let bid_data = self.bid_data.as_ref()?.clone();
        Some(PayloadHeaderData { payload_and_blobs: self.payload_and_blobs.clone(), bid_data })
    }
}

#[derive(Clone)]
pub struct PayloadHeaderData {
    pub payload_and_blobs: Arc<PayloadAndBlobs>,
    pub bid_data: PayloadBidData,
}

impl PayloadHeaderData {
    pub fn value(&self) -> &U256 {
        &self.bid_data.value
    }

    pub fn execution_payload(&self) -> &ExecutionPayload {
        &self.payload_and_blobs.execution_payload
    }

    pub fn block_hash(&self) -> &B256 {
        &self.execution_payload().block_hash
    }

    /// This may be slow because of the tx root
    pub fn into_builder_bid_slow(self) -> BuilderBid {
        let start = Instant::now();

        let header = self
            .payload_and_blobs
            .execution_payload
            .to_header(Some(self.bid_data.withdrawals_root), self.bid_data.tx_root);

        let bid = BuilderBid {
            header,
            blob_kzg_commitments: self.payload_and_blobs.blobs_bundle.commitments().clone(),
            value: self.bid_data.value,
            execution_requests: self.bid_data.execution_requests,
            pubkey: mock_public_key_bytes(),
        };

        BID_CREATION_LATENCY.observe(start.elapsed().as_micros() as f64);
        debug!("creating builder bid took {:?}", start.elapsed());

        bid
    }
}

#[derive(Clone)]
pub struct PayloadBidData {
    pub withdrawals_root: B256,
    pub tx_root: Option<B256>,
    pub execution_requests: Arc<ExecutionRequests>,
    pub value: U256,
}

pub enum SubWorkerJob {
    BlockSubmission {
        headers: http::HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace, // TODO: replace this with better tracing
        res_tx: oneshot::Sender<SubmissionResult>,
        span: tracing::Span,
        sent_at: Instant,
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
        submission: Submission,
        version: SubmissionVersion,
        merging_preferences: BlockMergingPreferences,
        withdrawals_root: B256,
        trace: SubmissionTrace,
        res_tx: oneshot::Sender<SubmissionResult>,
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
    // TODO: remove once we move merging to auctioneer
    GetBestPayloadForMerging {
        bid_slot: Slot,
        res_tx: oneshot::Sender<BestMergeablePayload>,
    },
    MergeRequest(BlockMergeRequest),
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
            Event::GetBestPayloadForMerging { .. } => "GetBestPayloadForMerging",
            Event::MergeRequest(_) => "MergeRequest",
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
