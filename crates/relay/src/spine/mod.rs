pub mod messages;

use flux::{communication::ShmemData, spine::SpineQueue, spine_derive::from_spine, tile::TileInfo};

#[from_spine("helix")]
#[derive(Debug)]
pub struct HelixSpine {
    pub tile_info: ShmemData<TileInfo>,

    #[queue(size(2usize.pow(16)))]
    pub filler: SpineQueue<messages::Tmp>,
}
