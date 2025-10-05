use std::time::Instant;

use alloy_primitives::B256;
use helix_common::{
    chain_info::ChainInfo, local_cache::LocalCache, record_submission_step, utils::utcnow_sec,
    GetPayloadTrace, RelayConfig,
};
use helix_types::{
    BlockMergingData, BlockMergingPreferences, BlsPublicKey, BlsPublicKeyBytes,
    DehydratedBidSubmission, ExecPayload, SigError, SignedBidSubmission,
    SignedBidSubmissionWithMergingData, SignedBlindedBeaconBlock, SignedValidatorRegistration,
};
use http::HeaderValue;
use tracing::{error, info, info_span, trace, warn};

use crate::{
    auctioneer::{
        decoder::SubmissionDecoder,
        types::{RegWorkerJob, SubWorkerJob, Submission},
        Event,
    },
    builder::{api::get_mergeable_orders, error::BuilderApiError},
    proposer::{MergingPoolMessage, ProposerApiError},
    HEADER_API_KEY, HEADER_HYDRATE, HEADER_IS_MERGEABLE, HEADER_SEQUENCE,
};

// TODO: spans
pub(super) struct SubWorker {
    pub(super) rx: crossbeam_channel::Receiver<SubWorkerJob>,
    pub(super) tx: crossbeam_channel::Sender<Event>,
    // TODO: move this to auctioneer
    pub(super) merge_pool_tx: tokio::sync::mpsc::Sender<MergingPoolMessage>,
    pub(super) cache: LocalCache,
    pub(super) chain_info: ChainInfo,
    pub(super) config: RelayConfig,
}

impl SubWorker {
    pub(super) fn run(self, id: usize) {
        let _span = info_span!("worker", %id).entered();
        info!("starting");

        loop {
            for task in self.rx.try_iter() {
                self.handle_task(task);
            }
        }
    }

    fn handle_task(&self, task: SubWorkerJob) {
        match task {
            SubWorkerJob::BlockSubmission { headers, body, trace, res_tx, span, sent_at } => {
                record_submission_step("worker_recv", sent_at.elapsed());

                let guard = span.enter();
                match self.handle_block_submission(headers, body) {
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
                match self.handle_get_payload(&proposer_pubkey, blinded_block, &mut trace) {
                    Ok((blinded, block_hash)) => {
                        let _ = self.tx.try_send(Event::GetPayload {
                            block_hash,
                            blinded,
                            trace,
                            res_tx,
                        });
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
    ) -> Result<(Submission, B256, Option<u64>, Option<BlockMergingData>), BuilderApiError> {
        trace!("handling block submission");

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

        let (submission, merging_data) = if should_hydrate {
            // caches are per builder and the builder pubkey is still unvalidated so we rely on the
            // api key pubkey for safety
            if !skip_sigverify {
                return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
            }

            let payload: DehydratedBidSubmission = decoder.decode(body)?;

            (Submission::Dehydrated(payload), None)
        } else {
            let (payload, merging_data) = if has_mergeable_data {
                let payload: SignedBidSubmissionWithMergingData = decoder.decode(body)?;
                (payload.submission, Some(payload.merging_data))
            } else {
                let payload: SignedBidSubmission = decoder.decode(body)?;
                (payload, None)
            };

            if !skip_sigverify {
                let start_sig = Instant::now();
                payload.verify_signature(self.chain_info.builder_domain)?;
                record_submission_step("signature", start_sig.elapsed());
            }

            payload.validate_payload_ssz_lengths(self.chain_info.max_blobs_per_block())?;
            (Submission::Full(payload), merging_data)
        };

        let withdrawals_root = submission.withdrawal_root();

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
    pub(super) rx: crossbeam_channel::Receiver<RegWorkerJob>,
    pub(super) chain_info: ChainInfo,
}

impl RegWorker {
    pub(super) fn run(self, id: usize) {
        let _span = info_span!("worker", %id).entered();
        info!("starting");

        while let Ok(task) = self.rx.recv() {
            self.handle_task(task);
        }
    }

    fn handle_task(&self, RegWorkerJob { regs, range, res_tx }: RegWorkerJob) {
        let mut res = Vec::with_capacity(range.len());
        for i in range {
            let valid = validate_registration(&self.chain_info, &regs[i]);
            res.push((i, valid.is_ok()));
        }

        let _ = res_tx.send(res);
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
