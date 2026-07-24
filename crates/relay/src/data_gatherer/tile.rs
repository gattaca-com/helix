use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use flux::{spine::SpineAdapter, tile::Tile, timing::InternalMessage};
use flux_utils::SharedVector;
use helix_common::{
    S3Config, api::builder_api::TopBidUpdate, config::ClickhouseConfig, decoder::Encoding,
};
use helix_types::{BlsPublicKeyBytes, Compression, MergeType};
use tracing::info;

use crate::{
    HelixSpine, SubmissionDataWithSpan,
    data_gatherer::{
        clickhouse::{BlockInfo, ClickhouseData},
        s3::S3Data,
    },
    spine::messages::{BidEvent, BidUpdate, DecodedSubmission, NewBidSubmission},
};

/// Per-slot counters, logged and reset on slot transition.
/// `extract_ok + extract_failed + compressed_skipped ==` raw
/// `NewBidSubmission` events seen this slot.
#[derive(Default)]
struct SlotStats {
    s3_uploads: u32,
    extract_ok: u32,
    extract_failed: u32,
    compressed_skipped: u32,
    decoded_seen: u32,
    bid_live_events: u32,
    top_bid_updates_seen: u32,
}

pub struct DataGatherer {
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    ch: Option<ClickhouseData>,
    s3: Option<S3Data>,
    current_slot: u64,
    rt: tokio::runtime::Runtime,
    stats: SlotStats,
}

impl DataGatherer {
    pub fn new(
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        instance_id: String,
        ch_config: Option<&ClickhouseConfig>,
        s3_config: Option<S3Config>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build data gatherer runtime");
        Self {
            decoded,
            ch: ch_config.map(|cfg| ClickhouseData::new(cfg, instance_id)),
            s3: s3_config.map(S3Data::new),
            current_slot: 0,
            rt,
            stats: SlotStats::default(),
        }
    }

    pub fn on_new_slot(&mut self, new_slot: u64) {
        self.report_slot_stats();
        self.current_slot = new_slot;
        if let Some(ch) = self.ch.as_mut() &&
            let Some(future) = ch.publish_snapshot(new_slot)
        {
            self.rt.spawn(future);
        }
    }

    fn report_slot_stats(&mut self) {
        if self.current_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut self.stats);
        info!(
            bid_slot = self.current_slot,
            s3_uploads = stats.s3_uploads,
            extract_ok = stats.extract_ok,
            extract_failed = stats.extract_failed,
            compressed_skipped = stats.compressed_skipped,
            decoded_seen = stats.decoded_seen,
            bid_live_events = stats.bid_live_events,
            top_bid_updates_seen = stats.top_bid_updates_seen,
            "data gatherer slot stats"
        );
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
                        u64::from_le_bytes(buf[0..8].try_into().unwrap()),
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
                let payload = &payload[bid.payload_offset..];
                if let Some(s3) = self.s3.as_ref() {
                    self.stats.s3_uploads += 1;
                    self.rt.spawn(s3.upload_task(bid.header, payload));
                }

                let is_mergeable = matches!(bid.header.merge_type, MergeType::Mergeable);
                if let Compression::None = bid.header.compression {
                    if let Some((slot, block_hash, builder_pubkey)) =
                        Self::extract_block_hash_and_pubkey(bid.header.encoding, payload, is_mergeable)
                    {
                        self.stats.extract_ok += 1;
                        max_slot = max_slot.max(slot);
                        if let Some(ch) = self.ch.as_mut() {
                            ch.insert(block_hash, BlockInfo {
                                builder_pubkey,
                                slot,
                                is_dehydrated: bid.header.flags.is_dehydrated(),
                                received_ns: bid.trace.receive_ns.0 as i64,
                                read_body_ns: bid.trace.read_body_ns.0 as i64,
                                ..Default::default()
                            });
                        }
                    } else {
                        self.stats.extract_failed += 1;
                        tracing::error!(
                            "failed to extract builder_pubkey & block hash from submission with id {}",
                            bid.header.id
                        );
                    }
                } else {
                    self.stats.compressed_skipped += 1;
                }
            },
            |_, _| {},
        );

        adapter.consume_internal_message(|msg: &mut InternalMessage<DecodedSubmission>, _| {
            if let Some(bid) = self.decoded.get(msg.ix) {
                self.stats.decoded_seen += 1;
                max_slot = max_slot.max(bid.submission_data.bid_slot());
                if let Some(ch) = self.ch.as_mut() {
                    let info =
                        ch.entry(*bid.submission_data.block_hash()).or_insert_with(|| BlockInfo {
                            builder_pubkey: *bid.submission_data.builder_pubkey(),
                            slot: bid.submission_data.bid_slot(),
                            is_dehydrated: bid.submission_data.decoder_params.is_dehydrated,
                            received_ns: bid.submission_data.trace.receive_ns.0 as i64,
                            read_body_ns: bid.submission_data.trace.read_body_ns.0 as i64,
                            decoded_ns: None,
                            live_ns: None,
                            top_bid_ns: None,
                        });
                    info.decoded_ns = Some(msg.ingestion_time().real().0 as i64);
                }
            }
        });

        adapter.consume_internal_message(|msg: &mut InternalMessage<BidUpdate>, _| {
            if let Some(ch) = self.ch.as_mut() &&
                let Some(info) = ch.get_mut(&msg.block_hash)
            {
                // todo @nina - will we ever need other events?
                #[allow(irrefutable_let_patterns)]
                if let BidEvent::Live = msg.event {
                    self.stats.bid_live_events += 1;
                    info.live_ns = Some(msg.ingestion_time().real().0 as i64);
                }
            }
        });

        adapter.consume_internal_message(|msg: &mut InternalMessage<TopBidUpdate>, _| {
            self.stats.top_bid_updates_seen += 1;
            max_slot = max_slot.max(msg.slot);
            if let Some(ch) = self.ch.as_mut() &&
                let Some(info) = ch.get_mut(&msg.block_hash)
            {
                info.top_bid_ns = Some(msg.ingestion_time().real().0 as i64);
            }
        });

        if max_slot > self.current_slot {
            self.on_new_slot(max_slot);
        }

        // drive spawned S3 and clickhouse tasks
        self.rt.block_on(async { tokio::time::sleep(Duration::from_micros(500)).await });
    }

    fn teardown(mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        self.loop_body(adapter);
        if let Some(mut ch) = self.ch &&
            let Some(fut) = ch.publish_snapshot(u64::MAX)
        {
            self.rt.block_on(fut);
        }
    }
}
