use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    bid_submission::BidSubmission,
    GetPayloadTrace, SubmissionTrace,
};
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{
    BlockMergingPreferences, BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ForkName,
    GetPayloadResponse, PayloadAndBlobs, SignedBidSubmission, SignedBlindedBeaconBlock, Slot,
    VersionedSignedProposal,
};
use tokio::sync::oneshot;

use crate::{
    auctioneer::simulator::manager::SimulationResult,
    builder::error::BuilderApiError,
    gossiper::types::BroadcastPayloadParams,
    proposer::{GetHeaderParams, ProposerApiError},
};

pub type SubmissionResult = Result<(), BuilderApiError>;
pub type GetHeaderResult = Result<BuilderBid, ProposerApiError>;
pub type GetPayloadResult = Result<GetPayloadResultData, ProposerApiError>;
pub type BestMergeablePayload = Option<(BuilderBid, Arc<PayloadAndBlobs>)>;

pub struct GetPayloadResultData {
    pub to_proposer: GetPayloadResponse,
    pub to_publish: VersionedSignedProposal,
    pub trace: GetPayloadTrace,
    pub fork: ForkName,
}

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

    pub fn withdrawal_root(&self) -> B256 {
        match self {
            Submission::Full(s) => s.withdrawals_root(),
            Submission::Dehydrated(s) => s.withdrawal_root(),
        }
    }
}

pub enum WorkerJob {
    BlockSubmission {
        headers: http::HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace, // TODO: replace this with better tracing
        res_tx: oneshot::Sender<SubmissionResult>,
    },

    GetPayload {
        blinded_block: SignedBlindedBeaconBlock,
        proposer_pubkey: BlsPublicKeyBytes,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
    },
}

#[derive(Clone)]
pub struct SlotData {
    /// Head slot + 1, builders are bidding to build this slot
    pub bid_slot: Slot,
    /// Data about the validator registration
    pub registration_data: BuilderGetValidatorsResponseEntry,
    /// Payload attributes for the incoming blocks
    pub payload_attributes: PayloadAttributesUpdate,
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
        merging_preferences: BlockMergingPreferences,
        withdrawals_root: B256,
        sequence: Option<u64>,
        trace: SubmissionTrace,
        res_tx: oneshot::Sender<SubmissionResult>,
    },
    /// Assume already some validation (so we don't have to wait here)
    /// timing games already done
    GetHeader {
        params: GetHeaderParams,
        res_tx: oneshot::Sender<GetHeaderResult>,
    },
    // Receive multiple of these potentially, assume some light validation
    GetPayload {
        block_hash: B256,
        blinded: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
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
}
