use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use flux::{
    tile::{Tile, TileName},
    utils::{ShortTypename, short_typename},
};
use flux_profiler::timed;
use flux_utils::SharedVector;
use helix_common::{chain_info::ChainInfo, utils::utcnow_ns};
use helix_types::{SignedRoot, SignedValidatorRegistration, gloas::BlsBatch};
use tracing::info;

use crate::{
    HelixSpine, api::proposer::ProposerApiError, housekeeper::SlotUpdate,
    registration::handle::RegWorkerJob, spine::messages::SlotMsg,
};

/// `registrations_seen == valid + invalid`; `batches_seen == completed + aborted`.
#[derive(Default)]
struct RegStats {
    valid: u32,
    invalid: u32,
    completed: u32,
    aborted: u32,
}

struct Telemetry {
    work: Duration,
    spin: Duration,
    next_record: Instant,
    loop_start: Instant,
    loop_worked: Duration,
}

impl Telemetry {
    const REPORT_FREQ: Duration = Duration::from_millis(500);

    fn telemetry<T>(&mut self, id: &str, queue_type: &str, rx: &crossbeam_channel::Receiver<T>) {
        use helix_common::metrics::{WORKER_QUEUE_LEN, WORKER_UTIL};

        let now = Instant::now();
        let loop_elapsed = now.duration_since(self.loop_start);

        if loop_elapsed.is_zero() {
            return;
        }

        let worked = std::cmp::min(self.loop_worked, loop_elapsed);
        self.work += worked;
        self.spin += loop_elapsed - worked;

        self.loop_worked -= worked;
        self.loop_start = now;

        if self.next_record < now {
            self.next_record = now + Self::REPORT_FREQ;
            let spin = std::mem::take(&mut self.spin);
            let work = std::mem::take(&mut self.work);

            let total = spin + work;
            let util = if total.is_zero() { 0.0 } else { work.as_secs_f64() / total.as_secs_f64() };

            WORKER_UTIL.with_label_values(&[id]).observe(util);
            WORKER_QUEUE_LEN.with_label_values(&[queue_type]).observe(rx.len() as f64);
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            work: Default::default(),
            spin: Default::default(),
            next_record: Instant::now() +
                Self::REPORT_FREQ +
                Duration::from_millis(utcnow_ns() % 10 * 5),
            loop_start: Instant::now(),
            loop_worked: Default::default(),
        }
    }
}

pub struct RegistrationTile {
    core_id: usize,
    id: ShortTypename,
    chain_info: ChainInfo,
    tel: Telemetry,
    rx: crossbeam_channel::Receiver<RegWorkerJob>,
    slot_events: Arc<SharedVector<SlotUpdate>>,
    bid_slot: u64,
    stats: RegStats,
    batch: BlsBatch,
}

impl RegistrationTile {
    pub fn new(
        core_id: usize,
        chain_info: ChainInfo,
        rx: crossbeam_channel::Receiver<RegWorkerJob>,
        slot_events: Arc<SharedVector<SlotUpdate>>,
    ) -> Self {
        let id = ShortTypename::from_str_truncate(&format!("registration_{core_id}"));
        Self {
            core_id,
            id,
            chain_info,
            tel: Default::default(),
            rx,
            slot_events,
            bid_slot: 0,
            stats: RegStats::default(),
            batch: BlsBatch::default(),
        }
    }

    fn on_slot_msg(&mut self, msg: SlotMsg) {
        let Some(ev) = self.slot_events.get(msg.slot_update_id) else { return };
        let bid_slot = ev.bid_slot.as_u64();
        if bid_slot <= self.bid_slot {
            return;
        }
        self.report_slot_stats();
        self.bid_slot = bid_slot;
    }

    fn report_slot_stats(&mut self) {
        if self.bid_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut self.stats);
        info!(
            id = &*self.id,
            bid_slot = self.bid_slot,
            registrations_seen = stats.valid + stats.invalid,
            valid = stats.valid,
            invalid = stats.invalid,
            batches_seen = stats.completed + stats.aborted,
            completed = stats.completed,
            aborted = stats.aborted,
            "registration slot stats"
        );
    }

    fn handle_reg_task(&mut self, task: RegWorkerJob) {
        use helix_common::metrics::{WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US};

        let start_task = Instant::now();
        let outcome = self.process_reg_task(task);
        let tag = match outcome {
            Some(false) => "RegistrationBatch",
            Some(true) => "RegistrationBatch_Fallback",
            None => "RegistrationBatch_Aborted",
        };
        if outcome.is_some() {
            self.stats.completed += 1;
        } else {
            self.stats.aborted += 1;
        }

        let dur = start_task.elapsed();
        self.tel.loop_worked += dur;

        WORKER_TASK_COUNT.with_label_values(&[tag, &self.id]).inc();
        WORKER_TASK_LATENCY_US.with_label_values(&[tag, &self.id]).observe(dur.as_micros() as f64);
    }

    /// Returns `None` if cancellation is observed before verification; otherwise
    /// returns whether a multi-item check fell back, wrapped in `Some`.
    #[timed]
    fn process_reg_task(
        &mut self,
        RegWorkerJob { regs, range, res_tx }: RegWorkerJob,
    ) -> Option<bool> {
        use helix_common::metrics::WORKER_TASK_COUNT;

        if res_tx.is_closed() {
            return None;
        }
        let mut res = Vec::with_capacity(range.len());
        self.batch.clear();
        for i in range {
            let reg = &regs[i];
            let candidate = validate_timestamp(&self.chain_info, reg).is_ok();
            if candidate {
                let root = reg.message.signing_root(self.chain_info.builder_domain);
                self.batch.push(&reg.message.pubkey.0, &reg.signature.0, root);
            }
            res.push((i, candidate));
        }
        if res_tx.is_closed() {
            return None;
        }

        let fell_back = self.batch.verify();
        let candidates = res.iter_mut().filter(|(_, candidate)| *candidate);
        for ((_, valid), &verdict) in candidates.zip(self.batch.verdicts()) {
            *valid = verdict;
        }
        let valid = res.iter().filter(|(_, valid)| *valid).count() as u32;
        self.stats.valid += valid;
        self.stats.invalid += res.len() as u32 - valid;
        WORKER_TASK_COUNT.with_label_values(&["Registration", &self.id]).inc_by(res.len() as u64);

        let _ = res_tx.send(res);
        Some(fell_back)
    }
}

impl Tile<HelixSpine> for RegistrationTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|msg: SlotMsg, _| self.on_slot_msg(msg));

        if let Ok(task) = self.rx.recv_timeout(Duration::from_millis(50)) {
            self.handle_reg_task(task);
        }
        self.tel.telemetry(&self.id, "registration", &self.rx);
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate(&format!("{}_{}", short_typename::<Self>(), self.core_id))
    }
}

fn validate_timestamp(
    chain_info: &ChainInfo,
    registration: &SignedValidatorRegistration,
) -> Result<(), ProposerApiError> {
    use helix_common::utils::utcnow_sec;

    let ts = registration.message.timestamp;
    if ts < chain_info.genesis_time_in_secs {
        return Err(ProposerApiError::TimestampTooEarly {
            timestamp: ts,
            min_timestamp: chain_info.genesis_time_in_secs,
        });
    }
    if ts > utcnow_sec() + 10 {
        return Err(ProposerApiError::TimestampTooFarInTheFuture {
            timestamp: ts,
            max_timestamp: utcnow_sec() + 10,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use helix_common::utils::utcnow_sec;
    use helix_types::{ValidatorRegistration, get_fixed_pubkey_bytes, get_fixed_secret};
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn job_reports_each_registration_in_its_range() {
        let chain_info = ChainInfo::default();
        let registration = |key: usize, timestamp: u64, domain: B256| {
            let message = ValidatorRegistration {
                pubkey: get_fixed_pubkey_bytes(key),
                timestamp,
                ..Default::default()
            };
            let root = message.signing_root(domain);
            let signature = get_fixed_secret(key).sign(root).serialize().into();
            SignedValidatorRegistration { message, signature }
        };
        let (now, domain) = (utcnow_sec(), chain_info.builder_domain);
        let mut regs = vec![
            registration(0, now, domain),
            registration(1, chain_info.genesis_time_in_secs - 1, domain),
            registration(2, now, domain),
            registration(3, now, !domain),
            registration(4, now, domain),
        ];
        regs[2].message.gas_limit = 1;

        let (res_tx, mut res_rx) = oneshot::channel();
        let mut tile =
            RegistrationTile::new(0, chain_info, crossbeam_channel::never(), Default::default());
        // Driving the tile's loop requires a full spine; this tests the job handler.
        tile.handle_reg_task(RegWorkerJob { regs: Arc::new(regs), range: 1..5, res_tx });

        let mut res = res_rx.try_recv().expect("the job reports");
        res.sort_unstable();
        assert_eq!(res, [(1, false), (2, false), (3, false), (4, true)]);
    }
}
