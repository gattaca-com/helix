use std::time::{Duration, Instant};

use flux::{
    tile::{Tile, TileName},
    utils::{ShortTypename, short_typename},
};
use helix_common::{chain_info::ChainInfo, utils::utcnow_ns};
use helix_types::SignedValidatorRegistration;

use crate::{HelixSpine, api::proposer::ProposerApiError, registration::handle::RegWorkerJob};

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
}

impl RegistrationTile {
    pub fn new(
        core_id: usize,
        chain_info: ChainInfo,
        rx: crossbeam_channel::Receiver<RegWorkerJob>,
    ) -> Self {
        let id = ShortTypename::from_str_truncate(&format!("registration_{core_id}"));
        Self { core_id, id, chain_info, tel: Default::default(), rx }
    }

    fn handle_reg_task(&mut self, task: RegWorkerJob) {
        use helix_common::metrics::{WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US};

        let start_task = Instant::now();
        let completed = self.process_reg_task(task);
        let tag = if completed { "RegistrationBatch" } else { "RegistrationBatch_Aborted" };

        let dur = start_task.elapsed();
        self.tel.loop_worked += dur;

        WORKER_TASK_COUNT.with_label_values(&[tag, &self.id]).inc();
        WORKER_TASK_LATENCY_US.with_label_values(&[tag, &self.id]).observe(dur.as_micros() as f64);
    }

    /// Returns whether the task completed (false = caller dropped the result channel).
    fn process_reg_task(&self, RegWorkerJob { regs, range, res_tx }: RegWorkerJob) -> bool {
        use helix_common::metrics::{WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US};

        let mut res = Vec::with_capacity(range.len());

        for i in range {
            if res_tx.is_closed() {
                return false;
            }

            let start = Instant::now();
            let valid = validate_registration(&self.chain_info, &regs[i]);
            res.push((i, valid.is_ok()));

            WORKER_TASK_COUNT.with_label_values(&["Registration", &self.id]).inc();
            WORKER_TASK_LATENCY_US
                .with_label_values(&["Registration", &self.id])
                .observe(start.elapsed().as_micros() as f64);
        }

        let _ = res_tx.send(res);
        true
    }
}

impl Tile<HelixSpine> for RegistrationTile {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        if let Ok(task) = self.rx.recv_timeout(Duration::from_millis(50)) {
            self.handle_reg_task(task);
        }
        self.tel.telemetry(&self.id, "registration", &self.rx);
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate(&format!("{}_{}", short_typename::<Self>(), self.core_id))
    }
}

fn validate_registration(
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
    registration.verify_signature(chain_info.builder_domain)?;
    Ok(())
}
