use std::time::{Duration, Instant};

use flux::{spine::SpineAdapter, tile::Tile, timing::InternalMessage};
use flux_gather::{BlobCache, BlobShipper, BlobWriter};
use flux_network::tcp::TcpNetwork;
use flux_utils::ArrayStr;
use helix_common::{api::builder_api::TopBidUpdate, config::DataGatherConfig, gather::GatherMeta};
use helix_relay::{HelixSpine, NewBidSubmission, NewTcpBidSubmission, read_spine_epoch};
use helix_telemetry::{BidUpdate, DecodedSubmission, MergedBlockMsg, SimUpdate, SlotMsg};
use tracing::info;

use crate::{
    clickhouse::{BlockInfo, ClickhouseData},
    s3::S3Data,
};

/// Per-slot counters, logged and reset on slot transition.
#[derive(Default)]
struct SlotStats {
    s3_uploads: u32,
    decoded_seen: u32,
    bid_live_events: u32,
    top_bid_updates_seen: u32,
}

/// How long teardown drives queued uploads, inserts and blobs before giving up.
/// The spine signal grace (see the binary) must exceed this plus the disk drain.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

pub struct DataGatherer {
    ch: Option<ClickhouseData>,
    s3: Option<S3Data>,
    current_slot: u64,
    /// One poll behind the S3 and ClickHouse clients.
    net: TcpNetwork,
    stats: SlotStats,
    cache: BlobCache,
    shipper: BlobShipper,
    writer: BlobWriter,
    config: DataGatherConfig,
    instance: ArrayStr<64>,
    epoch: Option<String>,
    epoch_check_at: Instant,
}

impl DataGatherer {
    pub fn new(instance_id: String, config: DataGatherConfig) -> Self {
        let mut net = TcpNetwork::default();
        Self {
            ch: config
                .clickhouse
                .as_ref()
                .map(|cfg| ClickhouseData::new(cfg, instance_id.clone(), &mut net)),
            s3: config.s3.as_ref().map(|cfg| S3Data::new(cfg, &mut net)),
            current_slot: 0,
            net,
            stats: SlotStats::default(),
            cache: BlobCache::new(),
            // Bounded forwarding: a slow peer disconnects after holding the
            // backlog too long, and a dead peer sheds past the cap. The disk
            // copy (when configured) keeps what shipping drops.
            shipper: BlobShipper::new(config.addresses.clone())
                .with_max_backlog(4096, Duration::from_secs(30).into())
                .with_drop_outbound_backlog_on_disconnect(true),
            writer: BlobWriter::new(),
            config,
            instance: ArrayStr::from_str_truncate(&instance_id),
            epoch: read_spine_epoch(),
            epoch_check_at: Instant::now(),
        }
    }

    pub fn on_new_slot(&mut self, new_slot: u64) {
        self.flush();
        self.report_slot_stats();
        self.current_slot = new_slot;
        if let Some(ch) = self.ch.as_mut() {
            ch.publish_snapshot(new_slot);
        }
    }

    /// Polls the shared socket set, then lets each client send and retire its own requests.
    fn drive_clients(&mut self) -> bool {
        let Self { net, ch, s3, .. } = self;
        net.poll_with(|event| {
            if s3.as_mut().is_some_and(|s3| s3.on_event(&event)) {
                return;
            }
            if let Some(ch) = ch.as_mut() {
                ch.on_event(&event);
            }
        });
        let mut worked = s3.as_mut().is_some_and(|s3| s3.drive(net));
        worked |= ch.as_mut().is_some_and(|ch| ch.drive(net));
        worked
    }

    fn pending_requests(&self) -> usize {
        self.s3.as_ref().map_or(0, S3Data::pending) +
            self.ch.as_ref().map_or(0, ClickhouseData::pending)
    }

    fn flush(&mut self) {
        if self.cache.is_empty() {
            return;
        }
        let meta = GatherMeta::new(
            self.current_slot,
            self.cache.n_blobs() as u64,
            self.instance.as_str(),
            "helix",
        );
        let persist_dir = self.config.persist_dir.as_ref();
        let instance = self.instance;
        let slot = self.current_slot;
        self.cache.flush(&meta, 1, |blob| {
            self.shipper.ship(blob);
            if let Some(base) = persist_dir {
                // The writer truncates: a restart within the same slot must not
                // replace the earlier segment.
                let dir = base.join(instance.as_str()).join(blob.type_name());
                let mut path = dir.join(format!("{slot}.bin"));
                for n in 1.. {
                    if !path.exists() {
                        break;
                    }
                    path = dir.join(format!("{slot}.{n}.bin"));
                }
                self.writer.write(blob, &path);
            }
        });
    }

    fn report_slot_stats(&mut self) {
        if self.current_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut self.stats);
        let s3_failed = self.s3.as_mut().map_or(0, S3Data::take_failures);
        info!(
            bid_slot = self.current_slot,
            s3_uploads = stats.s3_uploads,
            s3_failed,
            decoded_seen = stats.decoded_seen,
            bid_live_events = stats.bid_live_events,
            top_bid_updates_seen = stats.top_bid_updates_seen,
            "data gatherer slot stats"
        );
    }

    fn on_new_bid(&mut self, bid: &NewBidSubmission, payload: &[u8]) {
        if let Some(s3) = self.s3.as_mut() {
            self.stats.s3_uploads += 1;
            s3.upload(bid.header, &payload[bid.payload_offset..]);
        }
    }

    /// Flushes the final slot and drives every sink until quiet or the
    /// deadline. Shared by teardown and the epoch-change exit.
    fn shutdown_drain(&mut self) {
        self.flush();
        if let Some(ch) = self.ch.as_mut() {
            ch.publish_snapshot(u64::MAX);
        }

        // Shipping, uploading and inserting all only queue, so the final slot leaves the
        // process only if something drives them afterwards. The shipper has no
        // pending count, so two quiet rounds end the drain.
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        let mut idle_rounds = 0;
        while Instant::now() < deadline {
            let ship_worked = self.shipper.drive();
            let clients_worked = self.drive_clients();
            if self.pending_requests() == 0 && !ship_worked && !clients_worked {
                idle_rounds += 1;
                if idle_rounds >= 2 {
                    break;
                }
            } else {
                idle_rounds = 0;
            }
        }
        self.writer.drain();
    }
}

impl Tile<HelixSpine> for DataGatherer {
    fn loop_body(&mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        // A relay restart wipes these mapped queues. A changed marker means
        // this process now reads deleted files: drain and exit for restart.
        if self.epoch_check_at.elapsed() >= Duration::from_secs(1) {
            self.epoch_check_at = Instant::now();
            if read_spine_epoch() != self.epoch {
                info!("spine epoch changed, exiting for restart");
                self.shutdown_drain();
                std::process::exit(0);
            }
        }

        // Slot boundary first: everything cached below lands in the new slot.
        // Only the housekeeper tick and top bids (both post-validation) advance
        // the slot; a builder-supplied slot could jump it past every real one.
        adapter.consume_internal_message(|msg: &mut InternalMessage<SlotMsg>, _| {
            if msg.slot > self.current_slot {
                self.on_new_slot(msg.slot);
            }
            self.cache.push(msg);
        });

        // Former `gather_into` traffic, now explicit: control and sim
        // lifecycle messages land in the cache unchanged.
        adapter.consume_internal_message(|msg: &mut InternalMessage<SimUpdate>, _| {
            self.cache.push(msg);
        });
        adapter.consume_internal_message(|msg: &mut InternalMessage<MergedBlockMsg>, _| {
            self.cache.push(msg);
        });

        adapter.consume_with_dcache_internal_message(
            |bid: &InternalMessage<NewTcpBidSubmission>, payload| {
                self.on_new_bid(&bid.inner, payload);
            },
            |_, _| {},
        );

        adapter.consume_with_dcache_internal_message(
            |bid: &InternalMessage<NewBidSubmission>, payload| {
                self.on_new_bid(bid, payload);
            },
            |_, _| {},
        );

        adapter.consume_internal_message(|msg: &mut InternalMessage<DecodedSubmission>, _| {
            self.stats.decoded_seen += 1;
            // A resubmitted block keeps the row of its first decode.
            if let Some(ch) = self.ch.as_mut() &&
                ch.get_mut(&msg.block_hash).is_none()
            {
                ch.insert(msg.block_hash, BlockInfo {
                    builder_pubkey: msg.builder_pubkey,
                    slot: msg.slot,
                    is_dehydrated: msg.is_dehydrated,
                    received_ns: msg.receive_ns.0 as i64,
                    read_body_ns: msg.read_body_ns.0 as i64,
                    decoded_ns: Some(msg.ingestion_time().real().0 as i64),
                    ..Default::default()
                });
            }
            self.cache.push(msg);
        });

        adapter.consume_internal_message(|msg: &mut InternalMessage<BidUpdate>, _| {
            if let Some(ch) = self.ch.as_mut() &&
                let Some(info) = ch.get_mut(&msg.block_hash)
            {
                self.stats.bid_live_events += 1;
                info.live_ns = Some(msg.ingestion_time().real().0 as i64);
            }
            self.cache.push(msg);
        });

        adapter.consume_internal_message(|msg: &mut InternalMessage<TopBidUpdate>, _| {
            self.stats.top_bid_updates_seen += 1;
            // Advance before caching: this record belongs to the new slot.
            if msg.slot > self.current_slot {
                self.on_new_slot(msg.slot);
            }
            if let Some(ch) = self.ch.as_mut() &&
                let Some(info) = ch.get_mut(&msg.block_hash)
            {
                info.top_bid_ns = Some(msg.ingestion_time().real().0 as i64);
            }
            self.cache.push(msg);
        });

        if self.shipper.drive() | self.writer.poll() | self.drive_clients() {
            adapter.mark_work();
        }
    }

    fn teardown(mut self, adapter: &mut SpineAdapter<HelixSpine>) {
        self.loop_body(adapter);
        self.shutdown_drain();
    }
}

#[cfg(test)]
mod tests {
    use flux::spine::SpineProducers as _;

    use super::*;

    #[test]
    fn slot_tick_advances_before_it_is_cached() {
        let dir = std::env::temp_dir().join(format!("helix-gather-manual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut spine = HelixSpine::new_with_base_dir(&dir, Some("gather-manual"));
        let mut tile = DataGatherer::new("test-instance".into(), Default::default());
        let mut adapter = SpineAdapter::connect_tile(&tile, &mut spine);

        // Prime the consumer cursor: flux consumers start at the live tail
        // and skip anything produced before their first consume call.
        adapter.consume_internal_message(|_: &mut InternalMessage<SlotMsg>, _| {});
        adapter.producers.produce(SlotMsg { slot_update_id: 0, slot: 9 });
        tile.loop_body(&mut adapter);

        assert_eq!(tile.current_slot, 9);
        assert!(!tile.cache.is_empty());

        drop(adapter);
        drop(spine);
        std::fs::remove_dir_all(&dir).ok();
    }
}
