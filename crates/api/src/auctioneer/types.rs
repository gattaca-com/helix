use std::{sync::Arc, time::Instant};

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
    },
    bid_submission::BidSubmission,
    GetPayloadTrace, SubmissionTrace,
};
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{
    BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ForkName, GetPayloadResponse,
    HydrationCache, PayloadAndBlobs, SignedBidSubmission, SignedBlindedBeaconBlock, Slot,
    VersionedSignedProposal,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::oneshot;

use crate::{
    auctioneer::{bid_sorter::BidSorter, simulator::manager::SimulationResult},
    builder::error::BuilderApiError,
    gossiper::types::BroadcastPayloadParams,
    proposer::{GetHeaderParams, ProposerApiError},
};

pub type SubmissionResult = Result<(), BuilderApiError>;
pub type GetHeaderResult = Result<BuilderBid, ProposerApiError>;
pub type GetPayloadResult = Result<GetPayloadResultData, ProposerApiError>;

pub struct GetPayloadResultData {
    pub to_proposer: GetPayloadResponse,
    pub to_publish: VersionedSignedProposal,
    pub trace: GetPayloadTrace,
    pub proposer_pubkey: BlsPublicKeyBytes,
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
        trace: GetPayloadTrace,
        res_tx: oneshot::Sender<GetPayloadResult>,
    },
}

// do we need coordinates here
pub struct SlotContext {
    /// Head slot + 1, builders are bidding to build this slot
    pub bid_slot: Slot,
    /// Data about the validator registration
    pub registration_data: BuilderGetValidatorsResponseEntry,
    /// Payload attributes for the incoming blocks
    pub payload_attributes: PayloadAttributesUpdate,
    /// Current fork
    pub current_fork: ForkName,
    pub il: Option<InclusionListWithKey>,
}

impl SlotContext {
    fn validate(&self) {
        assert_eq!(self.bid_slot, self.registration_data.slot);
        assert_eq!(self.bid_slot, self.payload_attributes.slot);
        if let Some(il) = self.il.as_ref() {
            assert_eq!(self.bid_slot, il.key.0)
        }
    }
}

// cpu "intensive" stuff is:
// parsing headers, decode, hydration, sig verify, withdrawals and tx root, mergeable orders,
// get payload stuff, re-signing get header, encoding requests to JSON

pub struct PendingPayload {
    pub block_hash: B256,
    pub blinded: SignedBlindedBeaconBlock,
    pub res_tx: oneshot::Sender<GetPayloadResult>,
    pub retry_at: Instant,
}

pub struct SortingData {
    pub slot: SlotContext,
    pub sort: BidSorter,
    pub seen_block_hashes: FxHashSet<B256>,
    pub sequence: FxHashMap<BlsPublicKeyBytes, u64>,
    pub hydration_cache: HydrationCache,
    pub payloads: FxHashMap<B256, Arc<PayloadAndBlobs>>,
    pub inclusion_list: Option<InclusionListWithMetadata>,
}

impl SlotContext {
    pub fn proposer_pubkey(&self) -> &BlsPublicKeyBytes {
        &self.registration_data.entry.registration.message.pubkey
    }
}

pub enum Event {
    Submission {
        submission: Submission,
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
    NewSlot,
    SimResult(SimulationResult),
    SimulatorSync {
        id: usize,
        is_synced: bool,
    },
}
