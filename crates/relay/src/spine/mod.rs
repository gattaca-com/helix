pub mod messages;

use flux::{communication::ShmemData, spine::SpineQueue, spine_derive::from_spine, tile::TileInfo};

#[from_spine("helix")]
#[derive(Debug)]
pub struct HelixSpine {
    pub tile_info: ShmemData<TileInfo>,

    // dcache capacity is `(size + 1) * mtu` rounded up to a power of two and a
    // payload must fit in `mtu`; a full block with blobs exceeds 2 MB.
    #[queue(size(2usize.pow(7)), mtu(3 * 1024 * 1024))]
    pub to_decode: SpineQueue<messages::NewBidSubmission>,

    #[queue(size(2usize.pow(7)), mtu(3 * 1024 * 1024))]
    pub to_decode_tcp_only: SpineQueue<messages::NewTcpBidSubmission>,

    #[queue(size(2usize.pow(16)))]
    pub bid_submission_result: SpineQueue<messages::SubmissionResultWithRef>,

    #[queue(size(2usize.pow(16)))]
    pub decoded: SpineQueue<messages::DecodedSubmission>,

    #[queue(size(2usize.pow(16)))]
    pub decoded_tcp_only: SpineQueue<messages::DecodedTcpSubmission>,

    /// Auctioneer → SimulatorTile.
    #[queue(size(2usize.pow(16)))]
    pub to_sim: SpineQueue<messages::ToSimMsg>,

    /// SimulatorTile → Auctioneer.
    #[queue(size(2usize.pow(16)))]
    pub from_sim: SpineQueue<messages::FromSimMsg>,

    /// BlockMergingTile → Auctioneer.
    #[queue(size(2usize.pow(10)))]
    pub merged_block: SpineQueue<messages::MergedBlockMsg>,

    /// Auctioneer → TopBidTile.
    #[queue(size(2usize.pow(16)))]
    pub top_bid: SpineQueue<messages::TopBidUpdate>,

    #[queue(size(2usize.pow(16)))]
    pub bid_update: SpineQueue<messages::BidUpdate>,

    /// HousekeeperTile → all consumers.
    #[queue(size(2usize.pow(6)))]
    pub housekeeper_slot: SpineQueue<messages::SlotMsg>,
}
