use std::time::{Duration, Instant};

use alloy_primitives::B256;
use flux::{
    tile::{Tile, TileName},
    utils::{ShortTypename, short_typename},
};
use helix_common::{
    GetPayloadTrace, RelayConfig, SubmissionTrace,
    chain_info::ChainInfo,
    local_cache::LocalCache,
    metrics::{WORKER_QUEUE_LEN, WORKER_TASK_COUNT, WORKER_TASK_LATENCY_US, WORKER_UTIL},
    record_submission_step,
    utils::{utcnow_ns, utcnow_sec},
};
use helix_types::{
    BidAdjustmentData, BlockMergingData, BlsPublicKey, BlsPublicKeyBytes, DehydratedBidSubmission,
    DehydratedBidSubmissionFuluWithAdjustments, ExecPayload, MergeType, MergeableOrdersWithPref,
    SigError, SignedBidSubmission, SignedBidSubmissionFuluWithAdjustments,
    SignedBidSubmissionWithMergingData, SignedBlindedBeaconBlock, SignedValidatorRegistration,
    SubmissionVersion,
};
use tracing::{error, trace};

use crate::{
    HelixSpine,
    api::{builder::error::BuilderApiError, proposer::ProposerApiError},
    auctioneer::{
        InternalBidSubmissionHeader,
        block_merger::get_mergeable_orders,
        decoder::SubmissionDecoder,
        types::{Event, RegWorkerJob, SubWorkerJob, Submission, SubmissionData},
    },
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
    rx: crossbeam_channel::Receiver<SubWorkerJob>,
    cache: LocalCache,
    chain_info: ChainInfo,
    tel: Telemetry,
    config: RelayConfig,
}

impl SubWorker {
    pub fn new(
        core_id: usize,
        tx: crossbeam_channel::Sender<Event>,
        rx: crossbeam_channel::Receiver<SubWorkerJob>,
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
    ) -> Self {
        let id = ShortTypename::from_str_truncate(&format!("submission_{core_id}"));
        Self { core_id, id, tx, rx, cache, chain_info, tel: Telemetry::default(), config }
    }

    fn _handle_task(&self, task: SubWorkerJob) {
        match task {
            SubWorkerJob::BlockSubmission {
                submission_ref,
                header,
                body,
                mut trace,
                res_tx,
                span,
                sent_at,
                expected_pubkey,
            } => {
                record_submission_step("worker_recv", sent_at.elapsed());
                let guard = span.enter();
                trace!("received by worker");
                match self.handle_block_submission(header, expected_pubkey, body, &mut trace) {
                    Ok((
                        submission,
                        withdrawals_root,
                        version,
                        merging_data,
                        bid_adjustment_data,
                    )) => {
                        tracing::Span::current()
                            .record("bid_slot", tracing::field::display(submission.bid_slot()));
                        tracing::Span::current()
                            .record("block_hash", tracing::field::display(submission.block_hash()));
                        tracing::Span::current().record(
                            "builder_pubkey",
                            tracing::field::display(submission.builder_pubkey()),
                        );

                        trace!("sending to auctioneer");
                        drop(guard);

                        let merging_data = if self.config.block_merging_config.is_enabled {
                            merging_data.and_then(|data| {
                                if let Submission::Full(ref signed_bid_submission) = submission {
                                    //TODO: split up mergeable order and submission processing to
                                    // avoid delaying the bid update
                                    match get_mergeable_orders(signed_bid_submission, &data) {
                                        Ok(orders) => Some(MergeableOrdersWithPref {
                                            allow_appending: data.allow_appending,
                                            orders,
                                        }),
                                        Err(err) => {
                                            error!(%err, "failed to process mergeable orders");
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        };

                        let submission_data = SubmissionData {
                            submission_ref,
                            submission,
                            version,
                            merging_data,
                            bid_adjustment_data,
                            withdrawals_root,
                            trace,
                        };

                        let message = Event::Submission {
                            submission_data,
                            res_tx,
                            span,
                            sent_at: Instant::now(),
                        };

                        if self.tx.try_send(message).is_err() {
                            error!("failed sending submission to auctioneer");
                        }
                    }

                    Err(err) => {
                        res_tx.try_send((submission_ref, Err(err)));
                    }
                }
            }

            SubWorkerJob::GetPayload {
                blinded_block,
                proposer_pubkey,
                mut trace,
                res_tx,
                span,
            } => {
                let guard = span.enter();
                trace!("received by worker");
                match self.handle_get_payload(&proposer_pubkey, *blinded_block, &mut trace) {
                    Ok((blinded, block_hash)) => {
                        trace!("sending to auctioneer");
                        drop(guard);

                        if self
                            .tx
                            .try_send(Event::GetPayload {
                                block_hash,
                                blinded: Box::new(blinded),
                                trace,
                                res_tx,
                                span,
                            })
                            .is_err()
                        {
                            error!("failed to send get_payload to auctioneer");
                        };
                    }
                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn handle_block_submission(
        &self,
        header: InternalBidSubmissionHeader,
        expected_pubkey: Option<BlsPublicKeyBytes>,
        body: bytes::Bytes,
        trace: &mut SubmissionTrace,
    ) -> Result<
        (Submission, B256, SubmissionVersion, Option<BlockMergingData>, Option<BidAdjustmentData>),
        BuilderApiError,
    > {
        let mut decoder = SubmissionDecoder::new(header.compression, header.encoding);
        let body = decoder.decompress(body)?;

        let with_mergeable_data = header.merge_type.is_some();
        let with_adjustments = header.flags.with_adjustments();
        let is_dehydrated = header.flags.is_dehydrated();

        let builder_pubkey = decoder.extract_builder_pubkey(body.as_ref(), with_mergeable_data)?;
        let skip_sigverify = if let Some(expected_pubkey) = expected_pubkey {
            if builder_pubkey != expected_pubkey {
                return Err(BuilderApiError::InvalidBuilderPubkey(expected_pubkey, builder_pubkey));
            }

            true
        } else {
            header
                .api_key
                .is_some_and(|api_key| self.cache.validate_api_key(&api_key, &builder_pubkey))
        };

        trace!(
            ?header.sequence_number,
            is_dehydrated,
            skip_sigverify,
            with_mergeable_data,
            with_adjustments,
            "processing payload"
        );

        let flags = DecodeFlags {
            skip_sigverify,
            merge_type: header.merge_type,
            with_adjustments,
            block_merging_dry_run: self.config.block_merging_config.is_dry_run,
        };

        let (submission, merging_data, bid_adjustment_data) = if is_dehydrated {
            decode_dehydrated(&mut decoder, body, trace, &self.chain_info, &flags)?
        } else if with_mergeable_data {
            decode_merge(&mut decoder, body, trace, &self.chain_info, &flags)?
        } else {
            decode_default(&mut decoder, body, trace, &self.chain_info, &flags)?
        };

        let withdrawals_root = submission.withdrawal_root();

        let version = SubmissionVersion::new(trace.receive, header.sequence_number);
        Ok((submission, withdrawals_root, version, merging_data, bid_adjustment_data))
    }

    fn handle_get_payload(
        &self,
        proposer_pubkey: &BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        _trace: &mut GetPayloadTrace,
    ) -> Result<(SignedBlindedBeaconBlock, B256), ProposerApiError> {
        trace!("verifying signature");
        verify_signed_blinded_block_signature(&self.chain_info, &blinded_block, proposer_pubkey)?;
        trace!("signature verified");

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

impl Tile<HelixSpine> for SubWorker {
    fn loop_body(&mut self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        for task in self.rx.try_iter() {
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

struct DecodeFlags {
    skip_sigverify: bool,
    merge_type: MergeType,
    with_adjustments: bool,
    block_merging_dry_run: bool,
}

fn decode_dehydrated(
    decoder: &mut SubmissionDecoder,
    body: bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    if !flags.skip_sigverify {
        return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
    }

    let (submission, bid_adjustment) = if flags.with_adjustments {
        let sub_with_adjustment: DehydratedBidSubmissionFuluWithAdjustments =
            decoder.decode_by_fork(body, chain_info.current_fork_name())?;
        let (sub, adjustment_data) = sub_with_adjustment.split();

        (sub, Some(adjustment_data))
    } else {
        let submission: DehydratedBidSubmission =
            decoder.decode_by_fork(body, chain_info.current_fork_name())?;

        (submission, None)
    };

    trace.decoded = utcnow_ns();

    let merging_data = match flags.merge_type {
        MergeType::Mergeable => {
            //Should this return an error instead?
            error!("mergeable dehydrated submissions are not supported");
            None
        }
        MergeType::AppendOnly => Some(BlockMergingData::append_only(submission.fee_recipient())),
        MergeType::None => {
            if flags.block_merging_dry_run {
                Some(BlockMergingData::append_only(submission.fee_recipient()))
            } else {
                None
            }
        }
    };

    Ok((Submission::Dehydrated(submission), merging_data, bid_adjustment))
}

fn decode_merge(
    decoder: &mut SubmissionDecoder,
    body: bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    let sub_with_merging: SignedBidSubmissionWithMergingData = decoder.decode(body)?;
    let mut upgraded = sub_with_merging.maybe_upgrade_to_fulu(chain_info.current_fork_name());
    trace.decoded = utcnow_ns();
    let merging_data = match flags.merge_type {
        MergeType::Mergeable => Some(upgraded.merging_data),
        //Handle append-only by creating empty mergeable orders
        //this allows builder to switch between append-only and mergeable without changing
        // submission alternatively we could reject or ignore append-only here if the
        // submission is mergeable?
        MergeType::AppendOnly => Some(BlockMergingData {
            allow_appending: upgraded.merging_data.allow_appending,
            builder_address: upgraded.merging_data.builder_address,
            merge_orders: vec![],
        }),
        MergeType::None => Some(upgraded.merging_data),
    };
    verify_and_validate(&mut upgraded.submission, flags.skip_sigverify, chain_info)?;
    Ok((Submission::Full(upgraded.submission), merging_data, None))
}

fn decode_default(
    decoder: &mut SubmissionDecoder,
    body: bytes::Bytes,
    trace: &mut SubmissionTrace,
    chain_info: &ChainInfo,
    flags: &DecodeFlags,
) -> Result<(Submission, Option<BlockMergingData>, Option<BidAdjustmentData>), BuilderApiError> {
    let (submission, bid_adjustment) = if flags.with_adjustments {
        let sub_with_adjustment: SignedBidSubmissionFuluWithAdjustments = decoder.decode(body)?;
        let (sub, adjustment_data) = sub_with_adjustment.split();

        (sub, Some(adjustment_data))
    } else {
        let submission: SignedBidSubmission = decoder.decode(body)?;

        (submission, None)
    };

    let mut upgraded = submission.maybe_upgrade_to_fulu(chain_info.current_fork_name());
    trace.decoded = utcnow_ns();
    let merging_data = match flags.merge_type {
        MergeType::Mergeable => {
            //Should this return an error instead?
            error!("mergeable dehydrated submissions are not supported");
            None
        }
        MergeType::AppendOnly => Some(BlockMergingData::append_only(upgraded.fee_recipient())),
        MergeType::None => {
            if flags.block_merging_dry_run {
                Some(BlockMergingData::allow_all(upgraded.fee_recipient(), upgraded.num_txs()))
            } else {
                None
            }
        }
    };
    verify_and_validate(&mut upgraded, flags.skip_sigverify, chain_info)?;
    Ok((Submission::Full(upgraded), merging_data, bid_adjustment))
}

fn verify_and_validate(
    submission: &mut SignedBidSubmission,
    skip_sigverify: bool,
    chain_info: &ChainInfo,
) -> Result<(), BuilderApiError> {
    if !skip_sigverify {
        trace!("verifying signature");
        let start_sig = Instant::now();
        submission.verify_signature(chain_info.builder_domain)?;
        trace!("signature ok");
        record_submission_step("signature", start_sig.elapsed());
    }
    submission.validate_payload_ssz_lengths(chain_info.max_blobs_per_block())?;
    Ok(())
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
