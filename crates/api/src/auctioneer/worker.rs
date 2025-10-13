use std::time::{Duration, Instant};

use alloy_primitives::B256;
use helix_common::{
    GetPayloadTrace, RelayConfig, SubmissionTrace,
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::{WORKER_QUEUE_LEN, WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US, WORKER_UTIL},
    record_submission_step,
    utils::{utcnow_ns, utcnow_sec},
};
use helix_types::{
    BlockMergingData, BlockMergingPreferences, BlsPublicKey, BlsPublicKeyBytes,
    DehydratedBidSubmission, ExecPayload, SigError, SignedBidSubmission,
    SignedBidSubmissionWithMergingData, SignedBlindedBeaconBlock, SignedValidatorRegistration,
};
use http::HeaderValue;
use tracing::{error, info, info_span, trace, warn};

use crate::{
    HEADER_API_KEY, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_SEQUENCE,
    auctioneer::{
        Event,
        decoder::SubmissionDecoder,
        types::{RegWorkerJob, SubWorkerJob, Submission},
    },
    builder::{api::get_mergeable_orders, error::BuilderApiError},
    proposer::{MergingPoolMessage, ProposerApiError},
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
pub(super) struct SubWorker {
    id: String,
    tx: crossbeam_channel::Sender<Event>,
    // TODO: move this to auctioneer
    merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
    cache: LocalCache,
    chain_info: ChainInfo,
    config: RelayConfig,
    tel: Telemetry,
}

impl SubWorker {
    pub fn new(
        id: usize,
        tx: crossbeam_channel::Sender<Event>,
        merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
    ) -> Self {
        Self {
            id: format!("submission_{id}"),
            tx,
            merge_pool_tx,
            cache,
            chain_info,
            config,
            tel: Telemetry::default(),
        }
    }

    pub(super) fn run(mut self, rx: crossbeam_channel::Receiver<SubWorkerJob>) {
        let _span = info_span!("worker", id = self.id).entered();
        info!("starting");

        loop {
            for task in rx.try_iter() {
                self.handle_task(task);
            }

            self.tel.telemetry(&self.id, "submission", &rx);
        }
    }

    fn handle_task(&mut self, task: SubWorkerJob) {
        let start_task = Instant::now();
        let task_name = task.as_str();

        self._handle_task(task);

        let task_dur = start_task.elapsed();
        self.tel.loop_worked += task_dur;

        WORKER_TASK_COUNT.with_label_values(&[task_name, &self.id]).inc();
        WORKER_TASK_LATENCY_US
            .with_label_values(&[task_name, &self.id])
            .observe(task_dur.as_micros() as f64);
    }

    fn _handle_task(&self, task: SubWorkerJob) {
        match task {
            SubWorkerJob::BlockSubmission { headers, body, mut trace, res_tx, span, sent_at } => {
                record_submission_step("worker_recv", sent_at.elapsed());
                let guard = span.enter();
                trace!("received by worker");
                match self.handle_block_submission(headers, body, &mut trace) {
                    Ok((submission, withdrawals_root, sequence, merging_data)) => {
                        let merging_preferences = merging_data
                            .as_ref()
                            .map(|m| BlockMergingPreferences { allow_appending: m.allow_appending })
                            .unwrap_or_default();

                        tracing::Span::current()
                            .record("bid_slot", tracing::field::display(submission.bid_slot()));
                        tracing::Span::current()
                            .record("block_hash", tracing::field::display(submission.block_hash()));
                        tracing::Span::current().record(
                            "builder_pubkey",
                            tracing::field::display(submission.builder_pubkey()),
                        );

                        drop(guard);

                        if self.config.block_merging_config.is_enabled {
                            let message = Event::Submission {
                                // TODO: move this to auctioneer, avoid clones
                                submission: submission.clone(),
                                merging_preferences,
                                withdrawals_root,
                                sequence,
                                trace,
                                res_tx,
                                span,
                                sent_at: Instant::now(),
                            };

                            if self.tx.try_send(message).is_err() {
                                error!("failed sending submisison to auctioneer");
                            }

                            if let Some(merging_data) = merging_data {
                                let Submission::Full(payload) = submission else {
                                    return;
                                };

                                let mergeable_orders =
                                    match get_mergeable_orders(&payload, merging_data) {
                                        Ok(orders) => orders,
                                        Err(err) => {
                                            warn!(%err, "failed to get mergeable orders");
                                            return;
                                        }
                                    };

                                if mergeable_orders.orders.is_empty() {
                                    return;
                                }

                                let message = MergingPoolMessage::new(&payload, mergeable_orders);
                                if let Err(err) = self.merge_pool_tx.try_send(message) {
                                    error!(?err, "failed to send mergeable orders to merging pool");
                                };
                            }
                        } else {
                            let message = Event::Submission {
                                submission,
                                merging_preferences,
                                withdrawals_root,
                                sequence,
                                trace,
                                res_tx,
                                span,
                                sent_at: Instant::now(),
                            };

                            if self.tx.try_send(message).is_err() {
                                error!("failed sending submisison to auctioneer");
                            }
                        }
                    }

                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }
            }

            SubWorkerJob::GetPayload { blinded_block, proposer_pubkey, mut trace, res_tx } => {
                match self.handle_get_payload(&proposer_pubkey, *blinded_block, &mut trace) {
                    Ok((blinded, block_hash)) => {
                        if self
                            .tx
                            .try_send(Event::GetPayload {
                                block_hash,
                                blinded: Box::new(blinded),
                                trace,
                                res_tx,
                            })
                            .is_err()
                        {
                            error!("failed sending get_payload to auctioneer");
                        };
                    }
                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }
            }
        }
    }

    fn handle_block_submission(
        &self,
        headers: http::HeaderMap,
        body: bytes::Bytes,
        trace: &mut SubmissionTrace,
    ) -> Result<(Submission, B256, Option<u64>, Option<BlockMergingData>), BuilderApiError> {
        let mut decoder = SubmissionDecoder::from_headers(&headers);
        let body = decoder.decompress(body)?;
        let has_mergeable_data = matches!(headers.get(HEADER_IS_MERGEABLE), Some(header) if header == HeaderValue::from_static("true"));
        let builder_pubkey = decoder.extract_builder_pubkey(body.as_ref(), has_mergeable_data)?;

        let skip_sigverify = headers
            .get(HEADER_API_KEY)
            .is_some_and(|key| self.cache.validate_api_key(key, &builder_pubkey));
        let should_hydrate = headers.get(HEADER_HYDRATE).is_some();
        let sequence = headers
            .get(HEADER_SEQUENCE)
            .and_then(|seq| seq.to_str().ok())
            .and_then(|seq| seq.parse::<u64>().ok());

        trace!(?sequence, should_hydrate, skip_sigverify, has_mergeable_data, "processing payload");
        let (submission, merging_data) = if should_hydrate {
            // caches are per builder and the builder pubkey is still unvalidated so we rely on the
            // api key pubkey for safety
            if !skip_sigverify {
                return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
            }

            let payload: DehydratedBidSubmission = decoder.decode(body)?;
            trace.decoded = utcnow_ns();

            (Submission::Dehydrated(payload), None)
        } else {
            let (payload, merging_data) = if has_mergeable_data {
                let payload: SignedBidSubmissionWithMergingData = decoder.decode(body)?;
                let payload = payload.maybe_upgrade_to_fulu(self.chain_info.current_fork_name());
                (payload.submission, Some(payload.merging_data))
            } else {
                let payload: SignedBidSubmission = decoder.decode(body)?;
                let payload = payload.maybe_upgrade_to_fulu(self.chain_info.current_fork_name());
                (payload, None)
            };

            trace.decoded = utcnow_ns();

            if !skip_sigverify {
                trace!("verifying signature");
                let start_sig = Instant::now();
                payload.verify_signature(self.chain_info.builder_domain)?;
                trace!("signature ok");
                record_submission_step("signature", start_sig.elapsed());
            }

            payload.validate_payload_ssz_lengths(self.chain_info.max_blobs_per_block())?;
            (Submission::Full(payload), merging_data)
        };

        trace!("computing withdrawals root");
        let withdrawals_root = submission.withdrawal_root();
        trace!("withdrawals root done");

        Ok((submission, withdrawals_root, sequence, merging_data))
    }

    fn handle_get_payload(
        &self,
        proposer_pubkey: &BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        _trace: &mut GetPayloadTrace,
    ) -> Result<(SignedBlindedBeaconBlock, B256), ProposerApiError> {
        verify_signed_blinded_block_signature(&self.chain_info, &blinded_block, proposer_pubkey)?;

        let block_hash = blinded_block
            .message()
            .body()
            .execution_payload()
            .map_err(|_| ProposerApiError::InvalidFork)? // this should never happen as post altair there's always an execution payload
            .block_hash()
            .0;

        Ok((blinded_block, block_hash))
    }
}

/// Worker to process registrations verifications
pub(super) struct RegWorker {
    id: String,
    chain_info: ChainInfo,
    tel: Telemetry,
}

impl RegWorker {
    pub(super) fn new(id: usize, chain_info: ChainInfo) -> Self {
        Self { id: format!("registration_{id}"), chain_info, tel: Default::default() }
    }

    pub(super) fn run(mut self, rx: crossbeam_channel::Receiver<RegWorkerJob>) {
        let _span = info_span!("worker", id = self.id).entered();
        info!("starting");

        loop {
            if let Ok(task) = rx.recv_timeout(Duration::from_millis(50)) {
                self.handle_task(task);
            }

            self.tel.telemetry(&self.id, "registration", &rx);
        }
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

fn verify_signed_blinded_block_signature(
    chain_info: &ChainInfo,
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    public_key: &BlsPublicKeyBytes,
) -> Result<(), SigError> {
    let uncompressed_public_key = BlsPublicKey::deserialize(public_key.as_slice())
        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let slot = signed_blinded_beacon_block.message().slot();
    let epoch = slot.epoch(chain_info.slots_per_epoch());
    let fork = chain_info.context.fork_at_epoch(epoch);

    let valid = signed_blinded_beacon_block.verify_signature(
        None,
        &uncompressed_public_key,
        &fork,
        chain_info.genesis_validators_root,
        &chain_info.context,
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
