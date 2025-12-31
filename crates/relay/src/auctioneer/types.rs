use std::{
    ops::{Deref, Range},
    sync::Arc,
    time::Instant,
};

use alloy_primitives::{B256, U256};
use helix_common::{
    GetPayloadTrace, SubmissionTrace, ValidatorPreferences,
    api::{
        builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
        proposer_api::GetHeaderParams,
    },
    metrics::BID_CREATION_LATENCY,
};
use helix_types::{
    BidAdjustmentData, BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ExecutionPayload,
    ExecutionRequests, ForkName, GetPayloadResponse, MergeableOrdersWithPref, PayloadAndBlobs,
    SignedBidSubmission, SignedBlindedBeaconBlock, SignedValidatorRegistration, Slot,
    SubmissionVersion, VersionedSignedProposal, mock_public_key_bytes,
};
use rustc_hash::FxHashMap;
use ssz_derive::{Decode, Encode};
use tokio::sync::oneshot;
use tracing::debug;

use crate::{
    api::{builder::error::BuilderApiError, proposer::ProposerApiError},
    auctioneer::{
        BlockMergeResult, BlockSimRequest, SimulatorRequest, simulator::manager::SimulationResult,
    },
    gossip::BroadcastPayloadParams,
    housekeeper::PayloadAttributesUpdate,
};

pub type SubmissionResult = Result<(), BuilderApiError>;
pub type GetHeaderResult = Result<PayloadEntry, ProposerApiError>;
pub type GetPayloadResult = Result<GetPayloadResultData, ProposerApiError>;

pub struct GetPayloadResultData {
    pub to_proposer: GetPayloadResponse,
    pub to_publish: VersionedSignedProposal,
    pub trace: GetPayloadTrace,
    pub fork: ForkName,
    pub bid: PayloadBidData,
}

pub struct SubmissionData {
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

#[derive(Clone)]
pub struct GossipedPayload {
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
    pub slot_data: SimSlotData,
    pub parent_beacon_block_root: Option<B256>,
}

#[derive(Clone)]
pub enum PayloadEntry {
    Submission(SubmissionPayload),
    Gossip(GossipedPayload),
}

impl PayloadEntry {
    pub fn new_submission(
        signed_bid_submission: SignedBidSubmission,
        withdrawals_root: B256,
        tx_root: Option<B256>,
        bid_adjustment_data: Option<BidAdjustmentData>,
        submission_version: SubmissionVersion,
        submission_trace: SubmissionTrace,
        slot_data: SimSlotData,
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
            slot_data,
            parent_beacon_block_root,
        })
    }

    pub fn new_gossip(payload_and_blobs: PayloadAndBlobs, bid_data: PayloadBidData) -> Self {
        Self::Gossip(GossipedPayload { payload_and_blobs, bid_data })
    }

    pub fn is_adjusted(&self) -> bool {
        if let Self::Submission(s) = self {
            return s.is_adjusted;
        }

        return false;
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

    pub fn bid_data(&self) -> PayloadBidData {
        match &self {
            Self::Submission(s) => PayloadBidData {
                withdrawals_root: s.withdrawals_root,
                tx_root: s.tx_root,
                execution_requests: s.signed_bid_submission.execution_requests().clone(),
                value: *s.signed_bid_submission.value(),
                builder_pubkey: *s.signed_bid_submission.builder_public_key(),
            },
            Self::Gossip(s) => s.bid_data.clone(),
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
                s.signed_bid_submission.execution_requests().clone(),
                s.signed_bid_submission.payload_and_blobs().blobs_bundle.commitments().clone(),
            ),
            Self::Gossip(s) => (
                None,
                None,
                s.bid_data.execution_requests.clone(),
                s.payload_and_blobs.blobs_bundle.commitments().clone(),
            ),
        };

        let header = self.execution_payload().clone().to_header(withdrawals_root, tx_root);

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

    pub fn into_sim_request(&self) -> Option<SimulatorRequest> {
        match &self {
            Self::Gossip(_) => None,
            Self::Submission(bid) => {
                let request = BlockSimRequest::new(
                    bid.slot_data.gas_limit,
                    &bid.signed_bid_submission,
                    bid.slot_data.validator_preferences.clone(),
                    bid.parent_beacon_block_root,
                    bid.slot_data.inclusion_list.clone(),
                );

                let request = SimulatorRequest {
                    request,
                    is_top_bid: true,
                    res_tx: None,
                    submission: bid.signed_bid_submission.clone(),
                    trace: bid.submission_trace.clone(),
                    tx_root: bid.tx_root,
                    version: bid.submission_version,
                };

                Some(request)
            }
        }
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

#[derive(Clone)]
pub struct SimSlotData {
    pub gas_limit: u64,
    pub validator_preferences: ValidatorPreferences,
    pub inclusion_list: Option<InclusionListWithMetadata>,
}

impl From<&SlotData> for SimSlotData {
    fn from(data: &SlotData) -> Self {
        Self {
            gas_limit: data.registration_data.entry.registration.message.gas_limit.clone(),
            validator_preferences: data.registration_data.entry.preferences.clone(),
            inclusion_list: data.il.clone(),
        }
    }
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
    MergeResult(BlockMergeResult),
    DryRunAdjustments,
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
            Event::DryRunAdjustments => "DryRunAdjustments",
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
