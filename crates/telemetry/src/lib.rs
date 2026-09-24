//! Helix telemetry gathered by the data gatherer. Kept free of relay
//! dependencies so external readers can decode it.
//! Every type's wire name carries the `Helix` app segment, which also names
//! its persisted slot directory.

use alloy_primitives::{Address, B256, FixedBytes, U256};
use bytes::Bytes;
use flux::{timing::Nanos, type_hash_derive::type_hash_lock};
use flux_utils::ArrayStr;
use flux_versioned_types::{VersionedLeaves, versioned_telemetry};
use ssz::Encode;
use uuid::Uuid;

pub type BlsPublicKeyBytes = FixedBytes<48>;

versioned_telemetry!(DecodedSubmission, persist = "Helix.Submission.Decoded" =>
    // references position in SharedVector<BidSubmission>
    #[derive(Default)]
    #[type_hash_lock(hash = 4194973371243792068)]
    DecodedSubmissionV1 {
        pub decoded_submission_id: usize,
        pub submission_id: Uuid,
        pub block_hash: B256,
        pub builder_pubkey: BlsPublicKeyBytes,
        pub slot: u64,
        pub is_dehydrated: bool,
        pub _pad: [u8; 7],
        pub receive_ns: Nanos,
        pub read_body_ns: Nanos,
    }
);

versioned_telemetry!(MergedBlockMsg, persist = "Helix.BlockMerge.Merged" =>
    /// BlockMergingTile → Auctioneer: position in `SharedVector<BlockMergeResponse>`.
    #[type_hash_lock(hash = 784414618418290702)]
    MergedBlockMsgV1 {
        pub merged_block_response_id: usize,
    }
);

versioned_telemetry!(BidUpdate, persist = "Helix.Bid.Live" =>
    /// The bid went live.
    #[type_hash_lock(hash = 17286624082534006565)]
    BidUpdateV1 {
        pub submission_id: Uuid,
        pub block_hash: B256,
    }
);

versioned_telemetry!(SlotMsg, persist = "Helix.System.Slot" =>
    /// HousekeeperTile → all consumers: position in `SharedVector<SlotUpdate>`.
    /// `slot` duplicates the update slot for cross-process consumers (the
    /// data gatherer), which cannot resolve `slot_update_id`.
    #[derive(Default)]
    #[type_hash_lock(hash = 11031138757150622736)]
    SlotMsgV1 {
        pub slot_update_id: usize,
        pub slot: u64,
    }
);

versioned_telemetry!(SimStarted, persist = "Helix.Sim.Started" =>
    // One simulated bid, flat per event. `Started` carries the identity;
    // `TxIncluded` and `Finished` join to it on `submission_id`.
    #[derive(Default)]
    #[type_hash_lock(hash = 11723598201095314052)]
    SimStartedV1 {
        pub submission_id: Uuid,
        pub block_hash: B256,
        pub merged: bool,
        pub is_top_bid: bool,
        pub _pad: [u8; 6],
    }
);

versioned_telemetry!(SimTxIncluded, persist = "Helix.Sim.TxIncluded" =>
    #[derive(Default)]
    #[type_hash_lock(hash = 1388036155661251463)]
    SimTxIncludedV1 {
        pub submission_id: Uuid,
        pub nonce: u64,
        pub hash: B256,
        pub sender: Address,
        pub to: Address,
        pub builder_payment: U256,
        pub index: u32,
        pub has_to: bool,
        pub _pad: [u8; 3],
    }
);

versioned_telemetry!(SimFinished, persist = "Helix.Sim.Finished" =>
    #[derive(Default)]
    #[type_hash_lock(hash = 4203843626743507220)]
    SimFinishedV1 {
        pub submission_id: Uuid,
        pub total_payment: U256,
        pub elapsed_us: u64,
        pub retried: bool,
        pub _pad: [u8; 7],
        pub error: ArrayStr<256>,
    }
);

/// Sim lifecycle family, queued on `sim_updates`. `Started` carries the
/// identity; `TxIncluded` and `Finished` join to it on `submission_id`.
#[repr(C)]
#[derive(Clone, Copy, Debug, VersionedLeaves)]
pub enum SimUpdate {
    Started(SimStarted),
    TxIncluded(SimTxIncluded),
    Finished(SimFinished),
}

impl SimUpdate {
    /// Joining id shared by every event of one simulation.
    pub fn submission_id(&self) -> Uuid {
        match *self {
            SimUpdate::Started(inner) => inner.submission_id,
            SimUpdate::TxIncluded(inner) => inner.submission_id,
            SimUpdate::Finished(inner) => inner.submission_id,
        }
    }
}

#[derive(Clone, Copy)]
pub enum TopBidPrecision {
    Millis,
    Nanos,
}

versioned_telemetry!(TopBidUpdate, persist = "Helix.Bid.Top" =>
    #[derive(Default)]
    #[type_hash_lock(hash = 14624851659296435061)]
    TopBidUpdateV1 {
        pub timestamp: u64,
        pub slot: u64,
        pub block_number: u64,
        pub block_hash: B256,
        pub parent_hash: B256,
        pub builder_pubkey: BlsPublicKeyBytes,
        pub fee_recipient: Address,
        pub _pad: [u8; 4],
        pub value: U256,
    }
);

impl TopBidUpdate {
    const SSZ_SIZE: usize = 188;

    pub fn as_ssz_bytes_with_precision(mut self, precision: TopBidPrecision) -> Bytes {
        match precision {
            TopBidPrecision::Nanos => self.as_ssz_bytes().into(),
            TopBidPrecision::Millis => {
                self.timestamp /= 1_000_000;
                self.as_ssz_bytes().into()
            }
        }
    }
}

impl Encode for TopBidUpdate {
    fn is_ssz_fixed_len() -> bool {
        true
    }

    fn ssz_fixed_len() -> usize {
        Self::SSZ_SIZE
    }

    fn ssz_bytes_len(&self) -> usize {
        Self::SSZ_SIZE
    }

    fn ssz_append(&self, buf: &mut Vec<u8>) {
        self.timestamp.ssz_append(buf);
        self.slot.ssz_append(buf);
        self.block_number.ssz_append(buf);
        self.block_hash.ssz_append(buf);
        self.parent_hash.ssz_append(buf);
        self.builder_pubkey.ssz_append(buf);
        self.fee_recipient.ssz_append(buf);
        self.value.ssz_append(buf);
    }
}
