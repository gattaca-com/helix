use flux::{
    communication::ShmemData, spine::SpineQueue, spine_derive::from_spine, tile::TileInfo,
    type_hash_derive::type_hash_lock,
};
use flux_versioned_types::versioned_struct;

// Minimal flux spine: the builder's only tile is the merging TCP server, and
// all cross-thread traffic is crossbeam channels. The spine still provides
// core pinning, lifecycle and panic propagation. `from_spine` requires at
// least one queue, hence the unused placeholder.
#[from_spine("helix_builder")]
#[derive(Debug)]
pub struct BuilderSpine {
    pub tile_info: ShmemData<TileInfo>,

    #[queue(size(2usize.pow(4)))]
    pub unused: SpineQueue<Heartbeat>,
}

versioned_struct!(Heartbeat =>
    /// Placeholder message; never produced.
    #[type_hash_lock(hash = 10473874028190775778)]
    HeartbeatV1 {
        pub nonce: u64,
    }
);
