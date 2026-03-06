use std::time::{Duration, Instant};

use flux::{
    tile::{Tile, TileName},
    utils::{ShortTypename, short_typename},
};
use helix_common::{
    chain_info::ChainInfo,
    metrics::{WORKER_QUEUE_LEN, WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US, WORKER_UTIL},
    utils::{utcnow_ns, utcnow_sec},
};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, ExecPayload, SigError, SignedBlindedBeaconBlock,
    SignedValidatorRegistration,
};
use tracing::{error, trace};

use crate::{
    HelixSpine,
    api::proposer::ProposerApiError,
    auctioneer::types::{Event, GetPayload, RegWorkerJob},
};

pub struct Telemetry {
    work: Duration,
    // spin or wait
    spin: Duration,
    next_record: Instant,
    loop_start: Instant,
    loop_worked: Duration,
}

impl Telemetry {
    const REPORT_FREQ: Duration = Duration::from_millis(500);

    fn telemetry<T>(&mut self, id: &str, queue_type: &str, rx: &crossbeam_channel::Receiver<T>) {
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
                Duration::from_millis(utcnow_ns() % 10 * 5), // to scatter worker reports
            loop_start: Instant::now(),
            loop_worked: Default::default(),
        }
    }
}

// TODO: spans
pub struct SubWorker {
    core_id: usize,
    id: ShortTypename,
    tx: crossbeam_channel::Sender<Event>,
    rx: crossbeam_channel::Receiver<GetPayload>,
    chain_info: ChainInfo,
    tel: Telemetry,
}

impl SubWorker {
    pub fn new(
        core_id: usize,
        tx: crossbeam_channel::Sender<Event>,
        rx: crossbeam_channel::Receiver<GetPayload>,
        chain_info: ChainInfo,
    ) -> Self {
        let id = ShortTypename::from_str_truncate(&format!("submission_{core_id}"));
        Self { core_id, id, tx, rx, chain_info, tel: Telemetry::default() }
    }

    fn handle_get_payload(&self, task: GetPayload) {
        let GetPayload { blinded_block, proposer_pubkey, trace, res_tx, span } = task;
        let guard = span.enter();
        trace!("received by worker");
        let block_hash: Result<_, ProposerApiError> = (|| {
            verify_signed_blinded_block_signature(
                &self.chain_info,
                &blinded_block,
                &proposer_pubkey,
            )?;
            blinded_block
                .message()
                .body()
                .execution_payload()
                .map_err(|_| ProposerApiError::InvalidFork)
                .map(|p| p.block_hash().0)
        })();
        match block_hash {
            Ok(block_hash) => {
                trace!("sending to auctioneer");
                drop(guard);
                if self
                    .tx
                    .try_send(Event::GetPayload {
                        block_hash,
                        blinded: Box::new(*blinded_block),
                        trace,
                        res_tx,
                        span,
                    })
                    .is_err()
                {
                    error!("failed to send get_payload to auctioneer");
                }
            }
            Err(err) => {
                let _ = res_tx.send(Err(err));
            }
        }
    }
}

impl Tile<HelixSpine> for SubWorker {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        for task in self.rx.try_iter() {
            let start_task = Instant::now();
            let task_name = task.as_str();

            self.handle_get_payload(task);

            let task_dur = start_task.elapsed();
            self.tel.loop_worked += task_dur;

            WORKER_TASK_COUNT.with_label_values(&[task_name, &self.id]).inc();
            WORKER_TASK_LATENCY_US
                .with_label_values(&[task_name, &self.id])
                .observe(task_dur.as_micros() as f64);
        }

        self.tel.telemetry(&self.id, "submission", &self.rx);
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate(&format!("{}_{}", short_typename::<Self>(), self.core_id))
    }
}

/// Worker to process registrations verifications
pub struct RegWorker {
    core_id: usize,
    id: ShortTypename,
    chain_info: ChainInfo,
    tel: Telemetry,
    rx: crossbeam_channel::Receiver<RegWorkerJob>,
}

impl RegWorker {
    pub fn new(
        core_id: usize,
        chain_info: ChainInfo,
        rx: crossbeam_channel::Receiver<RegWorkerJob>,
    ) -> Self {
        let id = ShortTypename::from_str_truncate(&format!("registration_{core_id}"));
        Self { core_id, id, chain_info, tel: Default::default(), rx }
    }

    fn handle_task(&mut self, task: RegWorkerJob) {
        let start_task = Instant::now();

        let completed = self._handle_task(task);
        let task_tag = if completed { "RegistrationBatch" } else { "RegistrationBatch_Aborted" };

        let task_dur = start_task.elapsed();
        self.tel.loop_worked += task_dur;

        WORKER_TASK_COUNT.with_label_values(&[task_tag, &self.id]).inc();
        WORKER_TASK_LATENCY_US
            .with_label_values(&[task_tag, &self.id])
            .observe(task_dur.as_micros() as f64);
    }

    /// Returns whether the task was completed
    fn _handle_task(&self, RegWorkerJob { regs, range, res_tx }: RegWorkerJob) -> bool {
        let mut res = Vec::with_capacity(range.len());

        for i in range {
            if res_tx.is_closed() {
                // validator dropped the request so no point in processing more
                // a single signature check takes 1-5ms so it's ok to check this every time
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

impl Tile<HelixSpine> for RegWorker {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        if let Ok(task) = self.rx.recv_timeout(Duration::from_millis(50)) {
            self.handle_task(task);
        }

        self.tel.telemetry(&self.id, "registration", &self.rx);
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate(&format!("{}_{}", short_typename::<Self>(), self.core_id))
    }
}

fn verify_signed_blinded_block_signature(
    chain_info: &ChainInfo,
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    public_key: &BlsPublicKeyBytes,
) -> Result<(), SigError> {
    let uncompressed_public_key = BlsPublicKey::deserialize(public_key.as_slice())
        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let slot = signed_blinded_beacon_block.message().slot();
    let epoch = slot.epoch(chain_info.slots_per_epoch());
    let fork = chain_info.spec.fork_at_epoch(epoch);

    let valid = signed_blinded_beacon_block.verify_signature(
        None,
        &uncompressed_public_key,
        &fork,
        chain_info.genesis_validators_root,
        &chain_info.spec,
    );

    if !valid {
        return Err(SigError::InvalidBlsSignature);
    }

    Ok(())
}

/// Validate a single registration.
fn validate_registration(
    chain_info: &ChainInfo,
    registration: &SignedValidatorRegistration,
) -> Result<(), ProposerApiError> {
    validate_registration_time(chain_info, registration)?;
    registration.verify_signature(chain_info.builder_domain)?;

    Ok(())
}

/// Validates the timestamp in a `SignedValidatorRegistration` message.
///
/// - Ensures the timestamp is not too early (before genesis time)
/// - Ensures the timestamp is not too far in the future (current time + 10 seconds).
fn validate_registration_time(
    chain_info: &ChainInfo,
    registration: &SignedValidatorRegistration,
) -> Result<(), ProposerApiError> {
    let registration_timestamp = registration.message.timestamp;
    let registration_timestamp_upper_bound = utcnow_sec() + 10;

    if registration_timestamp < chain_info.genesis_time_in_secs {
        return Err(ProposerApiError::TimestampTooEarly {
            timestamp: registration_timestamp,
            min_timestamp: chain_info.genesis_time_in_secs,
        });
    } else if registration_timestamp > registration_timestamp_upper_bound {
        return Err(ProposerApiError::TimestampTooFarInTheFuture {
            timestamp: registration_timestamp,
            max_timestamp: registration_timestamp_upper_bound,
        });
    }

    Ok(())
}
