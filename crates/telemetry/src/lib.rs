//! Helix telemetry gathered by the data gatherer. Kept free of relay
//! dependencies so external readers can decode it.
//! Every type's wire name carries the `Helix` app segment, which also names
//! its persisted slot directory.

use std::net::{IpAddr, Ipv6Addr};

use alloy_primitives::{Address, B256, FixedBytes, U256};
use bytes::Bytes;
use flux::{timing::Nanos, type_hash_derive::type_hash_lock};
use flux_utils::ArrayStr;
use flux_versioned_types::{ByteStable, TelemetrySchema, VersionedLeaves, versioned_telemetry};
use serde::{Deserialize, Serialize};
use ssz::Encode;
use type_hash_derive::TypeHash;
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
    #[type_hash_lock(hash = 3197928459100667086)]
    SimFinishedV1 {
        pub submission_id: Uuid,
        pub total_payment: U256,
        pub elapsed_us: u64,
        pub retried: bool,
        pub error: SimError,
        pub _pad: [u8; 6],
    }
);

versioned_telemetry!(SimPreviousSlotDrops, persist = "Helix.Sim.PreviousSlotDrops" =>
    /// Requests discarded when their `PreviousSlot` retention expires.
    #[derive(Default)]
    #[type_hash_lock(hash = 5726037604136785908)]
    SimPreviousSlotDropsV1 {
        /// Target slot of the expired requests; emitted after two observed slot rolls.
        pub slot: u64,
        pub current_slot: u64,
        pub queued_requests: u64,
        pub superseded_requests: u64,
    }
);

/// How a simulation ended: `BlockSimError`'s variants without their payloads,
/// `None` for a valid block. `as_str` gives the text form.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
    strum::IntoStaticStr,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
#[strum(serialize_all = "snake_case")]
pub enum SimError {
    #[default]
    None = 0,
    BlockValidationFailed = 1,
    InvalidTxRoot = 2,
    Timeout = 3,
    RpcError = 4,
    SendError = 5,
    NoSimulatorAvailable = 6,
    SimulationDropped = 7,
    HydrationMiss = 8,
    RelayHydrationFailed = 9,
    UnsupportedFork = 10,
    PayloadTooLarge = 11,
}

impl SimError {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Sim lifecycle family, queued on `sim_updates`. `Started` carries the
/// identity; `TxIncluded` and `Finished` join to it on `submission_id`.
#[repr(C)]
#[derive(Clone, Copy, Debug, VersionedLeaves, TypeHash)]
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

versioned_telemetry!(GetHeaderServed, persist = "Helix.Header.Served" =>
    /// A header sent to a proposer, from `getHeader` or a header stream.
    #[derive(Default)]
    #[type_hash_lock(hash = 9754085527104981848)]
    GetHeaderServedV1 {
        pub slot: u64,
        pub receive_ns: u64,
        pub best_bid_fetched_ns: u64,
        pub block_hash: B256,
        pub value: U256,
        pub builder_pubkey: BlsPublicKeyBytes,
        /// IPv4 addresses are stored IPv6-mapped; all zeros when unknown.
        pub proposer_ip: [u8; 16],
        pub is_mev_boost: bool,
        pub streamed: bool,
        pub _pad: [u8; 6],
    }

    /// Adds the proposer pubkey from the request path, zero when migrated from V1.
    #[derive(Default)]
    #[type_hash_lock(hash = 12626954008180730437)]
    GetHeaderServedV2 {
        add { pub proposer_pubkey: BlsPublicKeyBytes = BlsPublicKeyBytes::ZERO }
    }
);

impl GetHeaderServed {
    pub fn ip_octets(ip: Option<IpAddr>) -> [u8; 16] {
        match ip {
            Some(IpAddr::V4(ip)) => ip.to_ipv6_mapped().octets(),
            Some(IpAddr::V6(ip)) => ip.octets(),
            None => [0; 16],
        }
    }

    pub fn proposer_ip(&self) -> Option<IpAddr> {
        (self.proposer_ip != [0; 16]).then(|| Ipv6Addr::from(self.proposer_ip).to_canonical())
    }
}

versioned_telemetry!(GetHeaderTimedOut, persist = "Helix.Header.TimedOut" =>
    #[derive(Default)]
    #[type_hash_lock(hash = 15080204562971561971)]
    GetHeaderTimedOutV1 {
        pub slot: u64,
        pub planned_hold_ms: u64,
        pub deadline_ms: u64,
        pub proposer_pubkey: BlsPublicKeyBytes,
        pub proposer_ip: [u8; 16],
        pub user_agent: ArrayStr<32>,
        pub deadline_source: ArrayStr<24>,
        pub hold_completed: bool,
        pub _pad: [u8; 7],
    }
);

/// Proposer API call a `ProposerRequest` describes.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum ProposerRequestKind {
    #[default]
    GetHeader = 0,
    GetPayload = 1,
    GetPayloadV2 = 2,
    RegisterValidator = 3,
}

/// HTTP version the client spoke, as opposed to the one a load balancer forwarded with.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum ClientHttpVersion {
    #[default]
    Unknown = 0,
    Http10 = 1,
    Http11 = 2,
    Http2 = 3,
}

/// TLS version of the client's connection; `Plaintext` when it had none.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum ClientTlsVersion {
    #[default]
    Unknown = 0,
    Plaintext = 1,
    Tls12 = 2,
    Tls13 = 3,
    Other = 4,
}

/// How the request reached the load balancer.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum ClientPath {
    #[default]
    Unknown = 0,
    Direct = 1,
    Cdn = 2,
}

versioned_telemetry!(ProposerRequest, persist = "Helix.Proposer.Request" =>
    /// A `getHeader`, `getPayload` or `registerValidator` request as it arrived, whatever its
    /// outcome. The `conn_*`, `tls_*`, `http_version` and `path` fields describe the client's
    /// own connection and come from the load balancer in front of the relay; they are zero or
    /// `Unknown` when it reported nothing.
    #[derive(Default)]
    #[type_hash_lock(hash = 8570014037663774977)]
    ProposerRequestV1 {
        /// Zero when the request named no slot that could be read.
        pub slot: u64,
        pub receive_ns: u64,
        /// Unix ns at which the client's TCP connection was accepted, on the clock of the
        /// host that accepted it, which need not be this one. With `proposer_ip` it identifies
        /// the connection across its requests.
        pub conn_accepted_ns: u64,
        /// How long the connection had seen no handshake or request when this one arrived.
        pub conn_idle_us: u64,
        pub tls_handshake_us: u64,
        /// Unix ms the client stamped on the request (`Date-Milliseconds`), zero when absent.
        pub client_send_ms: u64,
        /// FNV-1a over the request's header names, newline separated, in the order the relay
        /// read them. Two clients that claim one user agent and differ here are not the same
        /// software.
        pub header_names_hash: u64,
        /// For `getPayload` the slot's scheduled proposer, zero when the slot is unknown. Zero
        /// for `registerValidator`, which names many.
        pub proposer_pubkey: BlsPublicKeyBytes,
        /// IPv4 addresses are stored IPv6-mapped; all zeros when unknown.
        pub proposer_ip: [u8; 16],
        /// `x-request-id`, or one the relay made up when the request had none. Joins the
        /// request to the records of its outcome.
        pub request_id: Uuid,
        pub user_agent: ArrayStr<64>,
        /// `Host` the client asked for.
        pub host: ArrayStr<32>,
        /// Body size the client declared, zero when it declared none.
        pub content_length: u32,
        /// 1-based index of the request on its connection, counting every request the
        /// connection carried and not only the ones reported here.
        pub conn_request: u32,
        /// Kernel smoothed round trip of the client's connection and its variance.
        pub conn_srtt_us: u32,
        pub conn_rttvar_us: u32,
        pub kind: ProposerRequestKind,
        pub http_version: ClientHttpVersion,
        pub tls_version: ClientTlsVersion,
        pub path: ClientPath,
        pub tls_resumed: bool,
        pub _pad: [u8; 3],
    }
);

impl ProposerRequest {
    /// The fingerprint stored in `header_names_hash`.
    pub fn hash_header_names<'a>(names: impl Iterator<Item = &'a str>) -> u64 {
        const PRIME: u64 = 0x0100_0000_01b3;
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for name in names {
            for byte in name.bytes().chain([b'\n']) {
                hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
            }
        }
        hash
    }
}

versioned_telemetry!(GetPayloadAnswered, persist = "Helix.Payload.Answered" =>
    /// The answer to a `getPayload` request whose blinded block decoded. A request that did
    /// not decode, or that the relay timed out or the client dropped before the answer, has a
    /// `ProposerRequest` and no record here.
    #[derive(Default)]
    #[type_hash_lock(hash = 2482946776748656692)]
    GetPayloadAnsweredV1 {
        pub slot: u64,
        pub receive_ns: u64,
        pub answered_ns: u64,
        /// Proposer index the blinded block names.
        pub proposer_index: u64,
        pub block_hash: B256,
        /// The slot's scheduled proposer, zero when the slot is unknown.
        pub proposer_pubkey: BlsPublicKeyBytes,
        /// IPv4 addresses are stored IPv6-mapped; all zeros when unknown.
        pub proposer_ip: [u8; 16],
        pub request_id: Uuid,
        /// HTTP status of the answer: 200 (v1) or 202 (v2) when the payload was delivered.
        pub status: u16,
        pub kind: ProposerRequestKind,
        pub _pad: [u8; 5],
    }
);

versioned_telemetry!(RegistrationsAnswered, persist = "Helix.Registration.Answered" =>
    /// The answer to a `registerValidator` request, with what became of its registrations.
    /// The counts stop where the request did: one refused before its registrations were read
    /// has them all zero. A request the relay timed out or the client dropped has a
    /// `ProposerRequest` and no record here.
    #[derive(Default)]
    #[type_hash_lock(hash = 17050140305689073032)]
    RegistrationsAnsweredV1 {
        /// Head slot when the request arrived.
        pub head_slot: u64,
        pub receive_ns: u64,
        pub answered_ns: u64,
        /// Newest registration timestamp in the request, in Unix seconds.
        pub newest_timestamp: u64,
        /// IPv4 addresses are stored IPv6-mapped; all zeros when unknown.
        pub proposer_ip: [u8; 16],
        pub request_id: Uuid,
        pub registrations: u32,
        /// For validators the relay does not know.
        pub unknown: u32,
        /// Same as the stored registration, or too close to it to store again. Their
        /// signatures were not checked.
        pub unchanged: u32,
        /// Refused by the api provider.
        pub rejected: u32,
        /// Signature did not verify.
        pub invalid: u32,
        pub saved: u32,
        /// For validators with a scheduled proposal; each has a `ProposerRegistered`.
        pub scheduled: u32,
        pub status: u16,
        pub _pad: [u8; 2],
    }
);

/// What became of one registration in a `registerValidator` request.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    TypeHash,
    ByteStable,
    TelemetrySchema,
)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum RegistrationOutcome {
    /// Sent to signature verification, and the request failed before the result was in.
    #[default]
    Unverified = 0,
    /// The validator is not known to the relay.
    UnknownValidator = 1,
    /// Same as the stored registration, or too close to it to store again. The signature
    /// was not checked.
    Unchanged = 2,
    /// Refused by the api provider.
    Rejected = 3,
    /// Signature did not verify.
    Invalid = 4,
    Saved = 5,
}

versioned_telemetry!(ProposerRegistered, persist = "Helix.Registration.Proposer" =>
    /// One registration, in a `registerValidator` request, for a validator with a scheduled
    /// proposal. Says which client registers a proposer shortly before it asks for a header.
    #[derive(Default)]
    #[type_hash_lock(hash = 11349975648605006311)]
    ProposerRegisteredV1 {
        /// The validator's next scheduled proposal.
        pub slot: u64,
        pub receive_ns: u64,
        /// Timestamp the validator signed, in Unix seconds.
        pub timestamp: u64,
        pub gas_limit: u64,
        pub proposer_pubkey: BlsPublicKeyBytes,
        pub fee_recipient: Address,
        /// IPv4 addresses are stored IPv6-mapped; all zeros when unknown.
        pub proposer_ip: [u8; 16],
        pub request_id: Uuid,
        pub outcome: RegistrationOutcome,
        pub _pad: [u8; 3],
    }
);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingerprint is stored, so it must not change with the build.
    #[test]
    fn header_names_hash_is_fnv1a_of_the_names_in_order() {
        assert_eq!(ProposerRequest::hash_header_names([].into_iter()), 0xcbf2_9ce4_8422_2325);
        // FNV-1a 64 of "a\n".
        assert_eq!(ProposerRequest::hash_header_names(["a"].into_iter()), 0x089b_dc07_b544_e7b2);
        assert_ne!(
            ProposerRequest::hash_header_names(["accept", "host"].into_iter()),
            ProposerRequest::hash_header_names(["host", "accept"].into_iter()),
        );
        assert_ne!(
            ProposerRequest::hash_header_names(["ab", "c"].into_iter()),
            ProposerRequest::hash_header_names(["a", "bc"].into_iter()),
        );
    }
}
