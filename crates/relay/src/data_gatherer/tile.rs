use std::sync::Arc;

use alloy_primitives::B256;
use flux::{spine::SpineAdapter, tile::Tile, timing::InternalMessage};
use flux_utils::SharedVector;
use helix_common::{
    S3Config, api::builder_api::TopBidUpdate, config::ClickhouseConfig, decoder::Encoding,
    task::block_on,
};
use helix_types::{BlsPublicKeyBytes, MergeType};
use rustc_hash::FxHashMap;

use crate::{
    HelixSpine, SubmissionDataWithSpan,
    data_gatherer::{
        clickhouse::{BlockInfoRow, ClickhouseData},
        s3::S3Data,
    },
    spine::messages::{BidEvent, BidUpdate, DecodedSubmission, NewBidSubmission},
};

#[derive(Default)]
struct BlockInfo {
    builder_pubkey: BlsPublicKeyBytes,
    slot: u64,
    is_dehydrated: bool,
    received_ns: i64,
    read_body_ns: i64,
    decoded_ns: Option<i64>,
    live_ns: Option<i64>,
    top_bid_ns: Option<i64>,
}

pub struct DataGatherer {
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    map: FxHashMap<B256, BlockInfo>,
    ch: Option<ClickhouseData>,
    s3: Option<S3Data>,
    current_slot: u64,
    instance_id: String,
}

impl DataGatherer {
    pub fn new(
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        instance_id: String,
        ch_config: Option<&ClickhouseConfig>,
        s3_config: Option<S3Config>,
    ) -> Self {
        Self {
            decoded,
            map: FxHashMap::with_capacity_and_hasher(5000, Default::default()),
            ch: ch_config.map(ClickhouseData::new),
            s3: s3_config.map(S3Data::new),
            current_slot: 0,
            instance_id,
        }
    }

    pub fn on_new_slot(&mut self, new_slot: u64) {
        self.current_slot = new_slot;

        if let Some(ch) = self.ch.as_mut() {
            let rows = self
                .map
                .extract_if(|_, v| v.slot < new_slot)
                .map(|(hash, info)| Self::make_row(self.instance_id.clone(), hash, info));
            block_on(ch.publish(rows));
        } else {
            self.map.retain(|_, v| v.slot >= new_slot);
        }

        if let Some(s3) = self.s3.as_mut() {
            block_on(s3.flush());
        }
    }

    fn make_row(instance_id: String, hash: B256, info: BlockInfo) -> BlockInfoRow {
        BlockInfoRow {
            instance_id,
            slot: info.slot,
            block_hash: hash.to_string(),
            is_dehydrated: info.is_dehydrated,
            received_ns: info.received_ns,
            read_body_ns: info.read_body_ns,
            decoded_ns: info.decoded_ns,
            live_ns: info.live_ns,
            top_bid_ns: info.top_bid_ns,
            builder_pubkey: info.builder_pubkey.to_string(),
        }
    }

    fn extract_block_hash_and_pubkey(
        encoding: Encoding,
        buf: &[u8],
        has_mergeable_data: bool,
    ) -> Option<(u64, B256, BlsPublicKeyBytes)> {
        match encoding {
            Encoding::Json => {
                #[derive(serde::Deserialize)]
                struct Outer {
                    submission: Bid,
                }
                #[derive(serde::Deserialize)]
                struct Bid {
                    message: Message,
                }
                #[derive(serde::Deserialize)]
                struct Message {
                    #[serde(with = "serde_utils::quoted_u64")]
                    slot: u64,
                    block_hash: B256,
                    builder_pubkey: BlsPublicKeyBytes,
                }

                let bid: Bid = if has_mergeable_data {
                    serde_json::from_slice::<Outer>(buf).ok()?.submission
                } else {
                    serde_json::from_slice(buf).ok()?
                };

                Some((bid.message.slot, bid.message.block_hash, bid.message.builder_pubkey))
            }
            Encoding::Ssz => {
                const BLOCK_HASH_OFFSET: usize = 8 + /* slot */
                32; /* parent_hash */
                const BUILDER_PUBKEY_OFFSET: usize = BLOCK_HASH_OFFSET + 32; /* block_hash */

                if buf.len() < BUILDER_PUBKEY_OFFSET + BlsPublicKeyBytes::len_bytes() {
                    return None;
                }

                let (slot, block_hash, builder_pubkey) = unsafe {
                    (
                        core::ptr::read_unaligned(buf.as_ptr() as *const u64),
                        core::ptr::read_unaligned(
                            buf.as_ptr().add(BLOCK_HASH_OFFSET) as *const B256
                        ),
                        core::ptr::read_unaligned(
                            buf.as_ptr().add(BUILDER_PUBKEY_OFFSET) as *const BlsPublicKeyBytes
                        ),
                    )
                };

                Some((slot, block_hash, builder_pubkey))
            }
        }
    }
}

impl Tile<HelixSpine> for DataGatherer {
    fn loop_body(&mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        let mut max_slot = self.current_slot;

        adapter.consume_with_dcache_internal_message(
            |bid: &InternalMessage<NewBidSubmission>, payload| {
                if let Some(s3) = self.s3.as_mut() {
                    s3.push(bid.header, payload, bid.payload_offset);
                }

                let is_mergeable = matches!(bid.header.merge_type, MergeType::Mergeable);
                if let Some((slot, block_hash, builder_pubkey)) =
                    Self::extract_block_hash_and_pubkey(bid.header.encoding, payload, is_mergeable)
                {
                    max_slot = max_slot.max(slot);

                    let info = BlockInfo {
                        builder_pubkey,
                        slot,
                        is_dehydrated: bid.header.flags.is_dehydrated(),
                        received_ns: bid.trace.receive_ns.0 as i64,
                        read_body_ns: bid.trace.read_body_ns.0 as i64,
                        ..Default::default()
                    };

                    self.map.insert(block_hash, info);
                } else {
                    tracing::error!(
                        "failed to extract builder_pubkey & block hash from submission with id {}",
                        bid.header.id
                    );
                }
            },
            |_, _| {},
        );

        adapter.consume_internal_message(|msg: &mut InternalMessage<DecodedSubmission>, _| {
            if let Some(bid) = self.decoded.get(msg.ix) {
                max_slot = max_slot.max(bid.submission_data.bid_slot());
                if let Some(info) = self.map.get_mut(bid.submission_data.block_hash()) {
                    info.decoded_ns = Some(msg.ingestion_time().real().0 as i64);
                }
            }
        });

        adapter.consume(|msg: BidUpdate, _| {
            if let Some(info) = self.map.get_mut(&msg.block_hash) {
                let BidEvent::Live(nanos) = msg.event;
                info.live_ns = Some(nanos.0 as i64);
            }
        });

        adapter.consume(|msg: TopBidUpdate, _| {
            max_slot = max_slot.max(msg.slot);
            if let Some(info) = self.map.get_mut(&msg.block_hash) {
                info.top_bid_ns = Some(msg.timestamp as i64);
            }
        });

        if max_slot > self.current_slot {
            self.on_new_slot(max_slot);
        }
    }

    fn teardown(mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        self.loop_body(adapter);
        if let Some(ch) = self.ch.as_mut() {
            let rows = self
                .map
                .into_iter()
                .map(|(hash, info)| Self::make_row(self.instance_id.clone(), hash, info));
            block_on(ch.publish(rows));
        }
        if let Some(s3) = self.s3.as_mut() {
            block_on(s3.flush());
        }
    }
}
