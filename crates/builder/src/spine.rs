use flux::{communication::ShmemData, spine::SpineQueue, spine_derive::from_spine, tile::TileInfo};

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

/// Placeholder message; never produced.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Heartbeat {
    pub nonce: u64,
}
