pub mod messages;

use std::path::PathBuf;

use flux::{
    communication::ShmemData,
    spine::{FluxSpine as _, SpineQueue},
    spine_derive::from_spine,
    tile::TileInfo,
};
use helix_common::RelayConfig;
use serde::Deserialize;

#[from_spine("helix")]
#[derive(Debug)]
pub struct HelixSpine {
    pub tile_info: ShmemData<TileInfo>,

    // 6 MiB max payload, 128 in flight: 1 GiB dcache.
    #[queue(size(2usize.pow(7)), mtu(6 * 1024 * 1024))]
    pub to_decode: SpineQueue<messages::NewBidSubmission>,

    // The listener reserves dcache for frames it never publishes (registrations,
    // rejected bids), which advances the dcache without lapping the queue. The
    // extra depth doubles the dcache (2 GiB) as slack for those.
    #[queue(size(2usize.pow(8)), mtu(6 * 1024 * 1024))]
    pub to_decode_tcp_only: SpineQueue<messages::NewTcpBidSubmission>,

    #[queue(size(2usize.pow(16)))]
    pub bid_submission_result: SpineQueue<messages::SubmissionResultWithRef>,

    #[queue(size(2usize.pow(16)))]
    pub decoded: SpineQueue<messages::DecodedSubmission>,

    /// Auctioneer → DataGatherer: sim lifecycle events as
    /// `messages::SimUpdate`. Lossy under pressure: a full ring
    /// overwrites the oldest event rather than stalling simulation.
    #[queue(size(2usize.pow(18)), gather)]
    pub sim_updates: SpineQueue<messages::SimUpdate>,

    /// BlockMergingTile → Auctioneer.
    #[queue(size(2usize.pow(10)), gather)]
    pub merged_block: SpineQueue<messages::MergedBlockMsg>,

    /// Auctioneer → TopBidTile.
    #[queue(size(2usize.pow(16)))]
    pub top_bid: SpineQueue<messages::TopBidUpdate>,

    #[queue(size(2usize.pow(16)))]
    pub bid_update: SpineQueue<messages::BidUpdate>,

    /// HousekeeperTile → all consumers.
    #[queue(size(2usize.pow(6)), gather)]
    pub housekeeper_slot: SpineQueue<messages::SlotMsg>,
}

#[derive(Deserialize)]
pub struct RelayConfigExt {
    #[serde(flatten)]
    pub config: RelayConfig,
    pub spine_config: Option<HelixSpineConfig>,
}

impl AsRef<RelayConfig> for RelayConfigExt {
    fn as_ref(&self) -> &RelayConfig {
        &self.config
    }
}

/// Marker of the current relay generation. The relay writes it at startup
/// after wiping the queues; the standalone data gatherer exits when it
/// changes, since its mappings then point at deleted files.
/// Both processes compute the same default-layout path.
pub fn spine_epoch_path() -> PathBuf {
    flux_utils::directories::shmem_dir(HelixSpine::app_name()).join("epoch")
}

/// Current generation marker, or `None` when the relay has not started yet.
pub fn read_spine_epoch() -> Option<String> {
    std::fs::read_to_string(spine_epoch_path()).ok()
}
