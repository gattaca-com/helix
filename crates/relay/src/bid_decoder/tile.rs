use std::{cell::RefCell, collections::HashMap, sync::Arc};

use alloy_primitives::B256;
use bytes::Bytes;
use flux::{
    spine::{DCacheRead, SpineProducers},
    tile::Tile,
    timing::{InternalMessage, Nanos},
};
use flux_profiler::timed;
use flux_utils::SharedVector;
use helix_common::{
    RelayConfig, SubmissionTrace,
    api::builder_api::MAX_PAYLOAD_LENGTH,
    chain_info::ChainInfo,
    decoder::{SubmissionDecoder, SubmissionDecoderParams},
    local_cache::LocalCache,
    record_submission_step,
};
use helix_types::{
    BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, MergeableOrders,
    MergeableOrdersWithPref, SignedBidSubmission, Submission, SubmissionVersion,
};
use rustc_hash::FxHashMap;
use tracing::{info, trace};
use zstd::zstd_safe::WriteBuf;

use crate::{
    HelixSpine,
    api::{FutureBidSubmissionResult, builder::error::BuilderApiError},
    auctioneer::{
        InternalBidSubmissionHeader, SubmissionData, SubmissionRef, get_mergeable_orders,
        send_submission_result,
    },
    bid_decoder::SubmissionDataWithSpan,
    housekeeper::SlotUpdate,
    spine::{
        HelixSpineProducers,
        messages::{DecodedSubmission, NewBidSubmission, SlotMsg},
    },
};

/// Per-slot decode outcomes. `decoded_ok + decode_errors.values().sum() ==`
/// total submissions the decoder saw this slot.
#[derive(Default)]
struct DecodeStats {
    decoded_ok: u32,
    decode_errors: FxHashMap<&'static str, u32>,
}

pub struct DecoderTile {
    chain_info: ChainInfo,
    cache: LocalCache,
    config: RelayConfig,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    http_submissions: Arc<SharedVector<Bytes>>,
    buffer: RefCell<Vec<u8>>,
    core: usize,
    slot_events: Arc<SharedVector<SlotUpdate>>,
    bid_slot: u64,
    stats: RefCell<DecodeStats>,
}

impl Tile<HelixSpine> for DecoderTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|msg: SlotMsg, _| self.on_slot_msg(msg));

        adapter.consume_with_dcache_collaborative_internal_message(
            |new_bid: &InternalMessage<NewBidSubmission>, dcache_payload| {
                // dcache bypass: the dcache slot can be mutated between publish and
                // consume, read the stable staged copy when one is present.
                let bytes;
                let payload = if let Some(b) = self.http_submissions.get(new_bid.http_submission_ix)
                {
                    bytes = b;
                    &bytes.as_slice()[new_bid.payload_offset..]
                } else {
                    &dcache_payload[new_bid.payload_offset..]
                };
                let sent_at = new_bid.tracking_timestamp().publish_t();
                DecoderTile::handle_block_submission(
                    &self.cache,
                    &self.chain_info,
                    &self.config,
                    &new_bid.submission_ref,
                    &new_bid.header,
                    payload,
                    &mut self.buffer.borrow_mut(),
                    new_bid.trace,
                    sent_at,
                    new_bid.expected_pubkey(),
                )
            },
            |res, producers| match res {
                DCacheRead::Ok((new_bid, result)) => {
                    self.record_decode_result(&result);
                    let sent_at = new_bid.tracking_timestamp().publish_t();
                    Self::handle_result(
                        &self.decoded,
                        &self.future_results,
                        result,
                        sent_at,
                        new_bid.submission_ref,
                        producers,
                    );
                }
                DCacheRead::NoRef(new_bid) => {
                    let Some(payload) = self.http_submissions.get(new_bid.http_submission_ix)
                    else {
                        tracing::error!(
                            "failed to find the payload for bid submission with id = {}",
                            new_bid.header.id
                        );
                        self.record_decode_result(&Err(BuilderApiError::InternalError));
                        return send_submission_result(
                            producers,
                            &self.future_results,
                            new_bid.submission_ref,
                            Err(BuilderApiError::InternalError),
                        );
                    };

                    let sent_at = new_bid.tracking_timestamp().publish_t();
                    let result = DecoderTile::handle_block_submission(
                        &self.cache,
                        &self.chain_info,
                        &self.config,
                        &new_bid.submission_ref,
                        &new_bid.header,
                        payload.as_slice(),
                        &mut self.buffer.borrow_mut(),
                        new_bid.trace,
                        sent_at,
                        new_bid.expected_pubkey(),
                    );
                    self.record_decode_result(&result);
                    Self::handle_result(
                        &self.decoded,
                        &self.future_results,
                        result,
                        sent_at,
                        new_bid.submission_ref,
                        producers,
                    );
                }
                DCacheRead::Lost(new_bid) => {
                    tracing::error!(
                        "dcache read failed for bid submission with id {}",
                        new_bid.header.id
                    );
                    self.record_decode_result(&Err(BuilderApiError::InternalError));
                    send_submission_result(
                        producers,
                        &self.future_results,
                        new_bid.submission_ref,
                        Err(BuilderApiError::InternalError),
                    );
                }
                DCacheRead::SpedPast => {
                    tracing::error!("submissions consumer got sped past");
                }
                DCacheRead::Empty => {}
            },
        );
    }

    fn try_init(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) -> bool {
        adapter.set_collaborative_group_dcache::<NewBidSubmission>("decoder");
        true
    }

    fn name(&self) -> flux::tile::TileName {
        let mut name = flux_utils::short_typename::<Self>();
        name.push_str_truncate(self.core.to_string().as_str());
        name
    }
}

impl DecoderTile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        http_submissions: Arc<SharedVector<Bytes>>,
        slot_events: Arc<SharedVector<SlotUpdate>>,
        core: usize,
    ) -> Self {
        Self {
            chain_info,
            cache,
            config,
            decoded,
            future_results,
            http_submissions,
            buffer: RefCell::new(Vec::with_capacity(MAX_PAYLOAD_LENGTH)),
            core,
            slot_events,
            bid_slot: 0,
            stats: RefCell::new(DecodeStats::default()),
        }
    }

    fn on_slot_msg(&mut self, msg: SlotMsg) {
        let Some(ev) = self.slot_events.get(msg.ix) else { return };
        let bid_slot = ev.bid_slot.as_u64();
        if bid_slot <= self.bid_slot {
            return;
        }
        self.report_slot_stats();
        self.bid_slot = bid_slot;
    }

    fn report_slot_stats(&self) {
        if self.bid_slot == 0 {
            return;
        }
        let stats = std::mem::take(&mut *self.stats.borrow_mut());
        let decode_errors: u32 = stats.decode_errors.values().sum();
        let errors_by_category = stats.decode_errors;
        info!(
            bid_slot = self.bid_slot,
            submissions_seen = stats.decoded_ok + decode_errors,
            decoded_ok = stats.decoded_ok,
            decode_errors,
            ?errors_by_category,
            "bid decoder slot stats"
        );
    }

    fn record_decode_result(
        &self,
        result: &Result<(SubmissionData, tracing::Span), BuilderApiError>,
    ) {
        let mut stats = self.stats.borrow_mut();
        match result {
            Ok(_) => stats.decoded_ok += 1,
            Err(e) => *stats.decode_errors.entry(error_category(e)).or_insert(0) += 1,
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all,
        fields(
        id = tracing::field::Empty,
        slot = tracing::field::Empty,
        builder_pubkey = tracing::field::Empty,
        builder_id = tracing::field::Empty,
        block_hash = tracing::field::Empty,
    ))]
    #[timed]
    fn handle_block_submission(
        cache: &LocalCache,
        chain_info: &ChainInfo,
        config: &RelayConfig,
        submission_ref: &SubmissionRef,
        header: &InternalBidSubmissionHeader,
        payload: &[u8],
        buffer: &mut Vec<u8>,
        mut trace: SubmissionTrace,
        sent_at: Nanos,
        expected_pubkey: Option<&BlsPublicKeyBytes>,
    ) -> Result<(SubmissionData, tracing::Span), BuilderApiError> {
        tracing::Span::current().record("id", tracing::field::display(header.id));
        record_submission_step("worker_recv", sent_at.elapsed());
        trace!("received by worker");
        let (
            submission,
            withdrawals_root,
            version,
            merging_data,
            bid_adjustment_data,
            decoder_params,
        ) = Self::try_handle_block_submission(
            cache,
            chain_info,
            config,
            header,
            expected_pubkey,
            payload,
            buffer,
            &mut trace,
        )?;

        tracing::Span::current().record("slot", tracing::field::display(submission.bid_slot()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(submission.block_hash()));
        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(submission.builder_pubkey()));

        trace!("sending to auctioneer");

        let merging_data = if config.block_merging_config.is_enabled {
            merging_data.and_then(|data| match &submission {
                Submission::Full(signed_bid_submission) => {
                    // TODO: split up mergeable order and submission processing to
                    // avoid delaying the bid update
                    match get_mergeable_orders(signed_bid_submission, &data) {
                        Ok(orders) => Some(MergeableOrdersWithPref {
                            allow_appending: data.allow_appending,
                            orders,
                            merge_orders: data.merge_orders,
                        }),
                        Err(err) => {
                            tracing::error!(%err, "failed to process mergeable orders");
                            None
                        }
                    }
                }
                // transactions aren't resolved yet, so orders referencing tx indices can't be
                // expanded into `orders` here for either append-only or mergeable dehydrated
                // submissions. `merge_orders` is carried through raw so the external
                // merge-builder tile (which hydrates independently) can still resolve them; the
                // local `BestMergeableOrders` pool never receives expanded orders for dehydrated
                // submissions.
                Submission::Dehydrated(_) => Some(MergeableOrdersWithPref {
                    allow_appending: data.allow_appending,
                    orders: MergeableOrders::new(data.builder_address, Vec::new(), HashMap::new()),
                    merge_orders: data.merge_orders,
                }),
            })
        } else {
            None
        };

        let submission_data = SubmissionData {
            submission_ref: *submission_ref,
            submission,
            version,
            merging_data,
            bid_adjustment_data,
            withdrawals_root,
            trace,
            decoder_params,
            is_pessimistic: header.flags.pessimistic(),
        };

        Ok((submission_data, tracing::Span::current()))
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    #[timed]
    fn try_handle_block_submission(
        cache: &LocalCache,
        chain_info: &ChainInfo,
        config: &RelayConfig,
        header: &InternalBidSubmissionHeader,
        expected_pubkey: Option<&BlsPublicKeyBytes>,
        payload: &[u8],
        buffer: &mut Vec<u8>,
        trace: &mut SubmissionTrace,
    ) -> Result<
        (
            Submission,
            B256,
            SubmissionVersion,
            Option<BlockMergingData>,
            Option<BidAdjustmentData>,
            SubmissionDecoderParams,
        ),
        BuilderApiError,
    > {
        let with_mergeable_data = header.merge_type.is_some();
        let with_adjustments = header.flags.with_adjustments();
        let is_dehydrated = header.flags.is_dehydrated();

        let decoder_params = SubmissionDecoderParams {
            compression: header.compression,
            encoding: header.encoding,
            is_dehydrated,
            merge_type: header.merge_type,
            with_mergeable_data,
            with_adjustments,
            block_merging_dry_run: config.block_merging_config.is_dry_run,
            fork_name: chain_info.current_fork_name(),
        };

        let mut decoder = SubmissionDecoder::new(&decoder_params);
        let (mut submission, merging_data, bid_adjustment_data) =
            decoder.decode(payload, buffer)?;

        trace.decoded_ns = Nanos::now();

        let builder_pubkey = *submission.builder_pubkey();
        let skip_sigverify = if let Some(expected_pubkey) = expected_pubkey {
            if builder_pubkey != *expected_pubkey {
                return Err(BuilderApiError::InvalidBuilderPubkey(*expected_pubkey, builder_pubkey));
            }

            true
        } else {
            !header.api_key.is_empty() && cache.validate_api_key(&header.api_key, &builder_pubkey)
        };

        match submission {
            Submission::Full(ref mut signed_bid_submission) => {
                verify_and_validate(signed_bid_submission, skip_sigverify, chain_info)?;
            }
            Submission::Dehydrated { .. } => {
                if !skip_sigverify &&
                    (header.api_key.is_empty() || !cache.contains_api_key(&header.api_key))
                {
                    return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
                }
            }
        }

        let withdrawals_root = submission.withdrawal_root();

        let sequence_number = header.has_sequence_number.then_some(header.sequence_number);
        trace!(
            ?sequence_number,
            is_dehydrated,
            skip_sigverify,
            with_mergeable_data,
            with_adjustments,
            "processed payload"
        );

        let version = SubmissionVersion::new(trace.receive_ns.0, sequence_number);
        Ok((
            submission,
            withdrawals_root,
            version,
            merging_data,
            bid_adjustment_data,
            decoder_params,
        ))
    }

    fn handle_result(
        decoded: &SharedVector<SubmissionDataWithSpan>,
        future_results: &Arc<SharedVector<FutureBidSubmissionResult>>,
        result: Result<(SubmissionData, tracing::Span), BuilderApiError>,
        sent_at: Nanos,
        submission_ref: SubmissionRef,
        producers: &mut HelixSpineProducers,
    ) {
        match result {
            Ok((submission, span)) => {
                let ix = decoded.push(SubmissionDataWithSpan {
                    submission_data: submission,
                    span,
                    sent_at,
                });
                producers.produce(DecodedSubmission { ix });
            }
            Err(e) => {
                send_submission_result(producers, future_results, submission_ref, Err(e));
            }
        }
    }
}

/// Coarse, low-cardinality bucket for a decode failure; several
/// `BuilderApiError` variants carry per-request data unsuited to a metric key.
fn error_category(err: &BuilderApiError) -> &'static str {
    match err {
        BuilderApiError::JsonDecodeError(_) => "json_decode",
        BuilderApiError::SszDecode(_) => "ssz_decode",
        BuilderApiError::IOError(_) => "io",
        BuilderApiError::PayloadDecode(_) => "payload_decode",
        BuilderApiError::BidValidation(_) => "bid_validation",
        BuilderApiError::SigError(_) => "sig_error",
        BuilderApiError::HydrationError(_) => "hydration",
        BuilderApiError::UntrustedBuilderOnDehydratedPayload => "untrusted_builder",
        BuilderApiError::InvalidBuilderPubkey(..) => "invalid_pubkey",
        BuilderApiError::InternalError => "internal_error",
        _ => "other",
    }
}

#[timed]
fn verify_and_validate(
    submission: &mut SignedBidSubmission,
    skip_sigverify: bool,
    chain_info: &ChainInfo,
) -> Result<(), BuilderApiError> {
    if !skip_sigverify {
        trace!("verifying signature");
        let start_sig = Nanos::now();
        submission.verify_signature(chain_info.builder_domain)?;
        trace!("signature ok");
        record_submission_step("signature", start_sig.elapsed());
    }
    submission.validate_payload_ssz_lengths(chain_info.max_blobs_per_block())?;
    Ok(())
}
