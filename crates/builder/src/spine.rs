//! Builder spine: the merging TCP server tile and the merge engine tile.
//!
//! Each direction is one dcache-backed queue of a flat header plus an SSZ body; one queue per
//! direction keeps arrival order (`SlotStart` before its slot's blocks). Flux queues have no
//! backpressure: a consumer stalled for a queue's depth is lapped and reads `Lost`/`SpedPast`.
//!
//! `to_engine` is flow controlled by [`EngineCursor`], so its dcache is sized by bytes in flight
//! rather than flux's `(depth + 1) * mtu`. `from_engine` is not; its dcache is
//! `(depth + 1) * mtu`, rounded up to a power of two.
//!
//! Flux names a queue's shared memory after its message type, truncated to a few characters of
//! the type's own name: queue types here must differ in their first six characters.

use std::sync::atomic::AtomicU64;

use flux::{
    communication::ShmemData,
    spine_derive::from_spine,
    tile::TileInfo,
    type_hash_derive::{TypeHash, type_hash_lock},
};
use flux_versioned_types::{ByteStable, versioned_struct};
use serde::{Deserialize, Serialize};

#[from_spine("helix_builder")]
#[derive(Debug)]
pub struct BuilderSpine {
    pub tile_info: ShmemData<TileInfo>,

    /// Server → engine. Depth and dcache bytes are set from the merging config at startup.
    #[queue(size(2usize.pow(6)), mtu(16 * 1024 * 1024))]
    pub to_engine: SpineQueue<ToEngine>,

    /// Engine → server. Drained every server loop, so shallow.
    #[queue(size(2usize.pow(4)), mtu(16 * 1024 * 1024))]
    pub from_engine: SpineQueue<FromEngine>,
}

/// Values are the slot recording's kind bytes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TypeHash, ByteStable)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum EventKind {
    /// Body: SSZ `RelayConfigV1`, already validated. Takes effect at the next `SlotStart`.
    RelayConfig = 0,
    /// Body: SSZ `SlotStartV1`.
    SlotStart = 1,
    /// No body; `slot` set.
    SlotEnd = 2,
    /// Body: SSZ `MergeableBlockV1`, decompressed.
    MergeableBlock = 3,
    /// No body. The active connection was replaced or lost; expect a full slot replay.
    ConnectionReset = 4,
}

versioned_struct!(ToEngine =>
    #[type_hash_lock(hash = 9066653719515086515)]
    ToEngineV1 {
        pub kind: EventKind,
        pub _pad: [u8; 7],
        /// Relay connection: bumps on every active-connection change. Never decreases.
        pub generation: u64,
        pub slot: u64,
        pub recv_ns: u64,
        /// Events published so far, this one included.
        pub seq: u64,
        /// [`charge`] published so far, this one included.
        pub charge: u64,
    }
);

/// Credit for `to_engine`. The engine stores the `seq` and `charge` of the last event it has
/// copied out (Release); the server publishes only while the events and dcache bytes in flight
/// fit the queue (Acquire), so it never overwrites a slot or payload the engine has not read.
#[repr(C, align(64))]
#[derive(Debug, Default)]
pub struct EngineCursor {
    pub seq: AtomicU64,
    pub charge: AtomicU64,
}

/// Upper bound on how far a `len`-byte payload advances the dcache: its 64-byte-aligned slot,
/// plus under `len` bytes skipped when it would straddle the ring's end (`DCache::reserve`).
pub const fn charge(len: usize) -> u64 {
    2 * len.next_multiple_of(64) as u64
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TypeHash, ByteStable)]
#[byte_stable(crate = "::flux_versioned_types::byte_stable")]
pub enum OutputKind {
    /// Body: SSZ `MergedBlockV1` with `response_id` 0; the server stamps it in place.
    Merged = 0,
    /// Body: SSZ `RejectV1`.
    Reject = 1,
}

versioned_struct!(FromEngine =>
    #[type_hash_lock(hash = 4309133942227997742)]
    FromEngineV1 {
        pub kind: OutputKind,
        pub _pad: [u8; 7],
        /// The generation of the events the output answers; the server drops stale ones.
        pub generation: u64,
    }
);

#[cfg(test)]
mod tests {
    use flux_utils::DCache;

    use super::charge;
    use crate::testing::TestSpine;

    /// Each write advances the dcache cursor by at most its `charge`, the ring-end skip
    /// included, so the charge in flight bounds the span from the oldest unread payload.
    #[test]
    fn charge_bounds_each_dcache_advance() {
        let ring = 1 << 16;
        let dcache = DCache::new(ring);
        let mut end = 0;
        for i in 0..10_000 {
            // Lengths that straddle the ring end at varying offsets.
            let len = 1 + (i * 7919) % (ring / 3);
            let r = dcache.write(len, |_| {}).unwrap();
            let next = r.offset + len.next_multiple_of(64);
            assert!((next - end) as u64 <= charge(len), "len {len} advanced {}", next - end);
            end = next;
        }
    }

    /// Two queues mapping one file would share memory.
    #[test]
    fn each_queue_maps_its_own_file() {
        let spine = TestSpine::new();
        let queues = spine.dir.join("helix_builder/shmem/queues");
        assert_eq!(std::fs::read_dir(queues).unwrap().count(), 2);
    }
}
