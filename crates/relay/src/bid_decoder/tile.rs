use std::{cell::RefCell, sync::Arc};

use alloy_primitives::B256;
use bytes::Bytes;
use flux::{
    spine::{DCacheRead, SpineDCacheConsumer, SpineProducers},
    tile::Tile,
    timing::{InternalMessage, Nanos},
};
use flux_profiler::timed;
use flux_utils::SharedVector;
use helix_common::{
    RelayConfig, SubmissionTrace,
    api::builder_api::MAX_PAYLOAD_LENGTH,
    chain_info::ChainInfo,
    decoder::{Encoding, SubmissionDecoder, SubmissionDecoderParams},
    local_cache::LocalCache,
    record_submission_step, record_submission_step_ns,
    utils::utcnow_ns,
};
use helix_types::{
    BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, Compression, MergeOrderFlags,
    MergeType, Order, SignedBidSubmission, Submission, SubmissionVersion,
};
use rustc_hash::FxHashMap;
use tracing::{info, trace};

use crate::{
    HelixSpine,
    api::{FutureBidSubmissionResult, builder::error::BuilderApiError},
    auctioneer::{
        InternalBidSubmissionHeader, SubmissionData, SubmissionRef, send_submission_result,
    },
    bid_decoder::SubmissionDataWithSpan,
    housekeeper::SlotUpdate,
    spine::{
        HelixSpineProducers,
        messages::{DecodedSubmission, NewBidSubmission, NewTcpBidSubmission, SlotMsg},
    },
};

/// Per-slot decode outcomes. `decoded_ok + decode_errors.values().sum() ==`
/// total submissions the decoder saw this slot.
#[derive(Default)]
struct DecodeStats {
    decoded_ok: u32,
    decode_errors: FxHashMap<&'static str, u32>,
    by_builder: FxHashMap<BlsPublicKeyBytes, BuilderDecodeStats>,
}

/// Per-slot wire options a builder used, counted after the body decoded and
/// before the block merging dry run rewrites `merging_data`.
#[derive(Default)]
struct BuilderDecodeStats {
    submissions: u32,
    ssz: u32,
    json: u32,
    zstd: u32,
    gzip: u32,
    dehydrated: u32,
    with_api_key: u32,
    with_adjustments: u32,
    mergeable: u32,
    append_only: u32,
    merge_paused: u32,
    with_merging_data: u32,
    allow_appending: u32,
    merge_orders: u32,
    bundle_orders: u32,
    latest_only_orders: u32,
    tx_orders_can_revert: u32,
    /// Order shapes that point outside what they can address. `txs` indexes the
    /// block body; `reverting_txs` and `dropping_txs` index `txs` itself.
    oob_tx_index: u32,
    oob_reverting_txs: u32,
    oob_dropping_txs: u32,
    /// `reverting_txs` holds the same values as `txs`, so it likely carries
    /// block indices instead of positions in the bundle.
    reverting_txs_eq_txs: u32,
    /// Every position in the bundle may revert.
    bundles_all_reverting: u32,
    bundles_empty: u32,
    /// Dehydrated payloads whose builder pubkey no lane authenticated.
    dehydrated_unbound: u32,
}

impl BuilderDecodeStats {
    fn record_bundle(
        &mut self,
        txs: &[usize],
        reverting_txs: &[usize],
        dropping_txs: &[usize],
        num_txs: usize,
    ) {
        self.bundle_orders += 1;

        if txs.is_empty() {
            self.bundles_empty += 1;
            return;
        }
        if txs.iter().any(|&i| i >= num_txs) {
            self.oob_tx_index += 1;
        }
        if reverting_txs.iter().any(|&i| i >= txs.len()) {
            self.oob_reverting_txs += 1;
        }
        if dropping_txs.iter().any(|&i| i >= txs.len()) {
            self.oob_dropping_txs += 1;
        }
        if reverting_txs == txs {
            self.reverting_txs_eq_txs += 1;
        }
        if reverting_txs.len() >= txs.len() && (0..txs.len()).all(|i| reverting_txs.contains(&i)) {
            self.bundles_all_reverting += 1;
        }
    }
}

impl DecodeStats {
    fn record_submission(
        &mut self,
        builder_pubkey: BlsPublicKeyBytes,
        header: &InternalBidSubmissionHeader,
        merging_data: Option<&BlockMergingData>,
        num_txs: usize,
        skip_sigverify: bool,
    ) {
        let stats = self.by_builder.entry(builder_pubkey).or_default();
        stats.submissions += 1;

        match header.encoding {
            Encoding::Ssz => stats.ssz += 1,
            Encoding::Json => stats.json += 1,
        }

        match header.compression {
            Compression::Zstd => stats.zstd += 1,
            Compression::Gzip => stats.gzip += 1,
            Compression::None => {}
        }

        if header.flags.is_dehydrated() {
            stats.dehydrated += 1;
            if !skip_sigverify {
                stats.dehydrated_unbound += 1;
            }
        }
        if !header.api_key.is_empty() {
            stats.with_api_key += 1;
        }
        if header.flags.with_adjustments() {
            stats.with_adjustments += 1;
        }

        match header.merge_type {
            MergeType::Mergeable => stats.mergeable += 1,
            MergeType::AppendOnly => stats.append_only += 1,
            MergeType::Pause => stats.merge_paused += 1,
            MergeType::None => {}
        }

        let Some(merging_data) = merging_data else { return };

        stats.with_merging_data += 1;
        if merging_data.allow_appending {
            stats.allow_appending += 1;
        }
        stats.merge_orders = stats
            .merge_orders
            .saturating_add(u32::try_from(merging_data.merge_orders.len()).unwrap_or(u32::MAX));
        for order in &merging_data.merge_orders {
            match order {
                Order::Tx(tx) => {
                    if tx.can_revert {
                        stats.tx_orders_can_revert += 1;
                    }
                    if tx.index >= num_txs {
                        stats.oob_tx_index += 1;
                    }
                }
                Order::Bundle(bundle) => stats.record_bundle(
                    &bundle.txs,
                    &bundle.reverting_txs,
                    &bundle.dropping_txs,
                    num_txs,
                ),
                Order::BundleV2(bundle) => {
                    stats.record_bundle(
                        &bundle.txs,
                        &bundle.reverting_txs,
                        &bundle.dropping_txs,
                        num_txs,
                    );
                    if bundle.flags.contains(MergeOrderFlags::LATEST_ONLY) {
                        stats.latest_only_orders += 1;
                    }
                }
            }
        }
    }
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
    lane: Lane,
}

#[derive(Clone, Copy)]
pub enum Lane {
    All,
    TcpOnly,
}

pub trait SubmissionMsg: 'static + Copy {
    fn bid(&self) -> &NewBidSubmission;
}

impl SubmissionMsg for NewBidSubmission {
    fn bid(&self) -> &NewBidSubmission {
        self
    }
}

impl SubmissionMsg for NewTcpBidSubmission {
    fn bid(&self) -> &NewBidSubmission {
        &self.0
    }
}

impl Tile<HelixSpine> for DecoderTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|msg: SlotMsg, _| self.on_slot_msg(msg));

        match self.lane {
            Lane::All => {
                self.consume::<NewTcpBidSubmission>(adapter);
                self.consume::<NewBidSubmission>(adapter);
            }
            Lane::TcpOnly => self.consume::<NewTcpBidSubmission>(adapter),
        }
    }

    fn try_init(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) -> bool {
        match self.lane {
            Lane::All => {
                adapter.set_collaborative_group_dcache::<NewTcpBidSubmission>("decoder_tcp_only");
                adapter.set_collaborative_group_dcache::<NewBidSubmission>("decoder");
            }
            Lane::TcpOnly => {
                adapter.set_collaborative_group_dcache::<NewTcpBidSubmission>("decoder_tcp_only")
            }
        }
        true
    }

    fn name(&self) -> flux::tile::TileName {
        let mut name = flux_utils::short_typename::<Self>();
        name.push_str_truncate(self.core.to_string().as_str());
        name
    }
}

impl DecoderTile {
    fn consume<T>(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>)
    where
        T: SubmissionMsg,
        <HelixSpine as flux::spine::FluxSpine>::Consumers: AsMut<SpineDCacheConsumer<T>>,
    {
        adapter.consume_with_dcache_collaborative_internal_message(
            |msg: &InternalMessage<T>, dcache_payload| {
                let new_bid = msg.bid();
                // dcache bypass: the dcache slot can be mutated between publish and
                // consume, read the stable staged copy when one is present.
                let bytes;
                let payload = if let Some(b) = self.http_submissions.get(new_bid.http_submission_ix)
                {
                    bytes = b;
                    &bytes[new_bid.payload_offset..]
                } else {
                    &dcache_payload[new_bid.payload_offset..]
                };
                let sent_at = msg.tracking_timestamp().publish_t();
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
                    &self.stats,
                )
            },
            |res, producers| match res {
                DCacheRead::Ok((msg, result)) => {
                    let new_bid = msg.bid();
                    self.record_decode_result(&result);
                    let sent_at = msg.tracking_timestamp().publish_t();
                    Self::handle_result(
                        &self.decoded,
                        &self.future_results,
                        result,
                        sent_at,
                        new_bid.submission_ref,
                        producers,
                    );
                }
                DCacheRead::NoRef(msg) => {
                    let new_bid = msg.bid();
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

                    let sent_at = msg.tracking_timestamp().publish_t();
                    let result = DecoderTile::handle_block_submission(
                        &self.cache,
                        &self.chain_info,
                        &self.config,
                        &new_bid.submission_ref,
                        &new_bid.header,
                        &payload,
                        &mut self.buffer.borrow_mut(),
                        new_bid.trace,
                        sent_at,
                        new_bid.expected_pubkey(),
                        &self.stats,
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
                DCacheRead::Lost(msg) => {
                    let new_bid = msg.bid();
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
        lane: Lane,
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
            lane,
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

        let mut by_builder: Vec<_> = stats.by_builder.into_iter().collect();
        by_builder.sort_unstable_by_key(|(_, s)| std::cmp::Reverse(s.submissions));
        for (builder_pubkey, s) in by_builder {
            let builder_id = self
                .cache
                .get_builder_info(&builder_pubkey)
                .and_then(|info| info.builder_id)
                .unwrap_or_default();
            info!(
                bid_slot = self.bid_slot,
                %builder_pubkey,
                builder_id,
                submissions = s.submissions,
                ssz = s.ssz,
                json = s.json,
                zstd = s.zstd,
                gzip = s.gzip,
                dehydrated = s.dehydrated,
                dehydrated_unbound = s.dehydrated_unbound,
                with_api_key = s.with_api_key,
                with_adjustments = s.with_adjustments,
                mergeable = s.mergeable,
                append_only = s.append_only,
                merge_paused = s.merge_paused,
                with_merging_data = s.with_merging_data,
                allow_appending = s.allow_appending,
                merge_orders = s.merge_orders,
                bundle_orders = s.bundle_orders,
                latest_only_orders = s.latest_only_orders,
                tx_orders_can_revert = s.tx_orders_can_revert,
                oob_tx_index = s.oob_tx_index,
                oob_reverting_txs = s.oob_reverting_txs,
                oob_dropping_txs = s.oob_dropping_txs,
                reverting_txs_eq_txs = s.reverting_txs_eq_txs,
                bundles_all_reverting = s.bundles_all_reverting,
                bundles_empty = s.bundles_empty,
                "bid decoder builder stats"
            );
        }
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
        stats: &RefCell<DecodeStats>,
    ) -> Result<(SubmissionData, tracing::Span), BuilderApiError> {
        tracing::Span::current().record("id", tracing::field::display(header.id));
        record_submission_step("worker_recv", sent_at.elapsed());
        record_submission_step_ns("recv_worker", trace.receive_ns.0, utcnow_ns());
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
            stats,
        )?;

        tracing::Span::current().record("slot", tracing::field::display(submission.bid_slot()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(submission.block_hash()));
        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(submission.builder_pubkey()));

        trace!("sending to auctioneer");

        // Carried through raw (index-based, unexpanded) for `BlockMergingTile`, which
        // resolves tx bytes and caches blob sidecars itself when forwarding to the merge
        // builder — no need to do that work here on the submission hot path.
        let merging_data =
            if config.block_merging_config.is_enabled && header.merge_type != MergeType::Pause {
                // Dry run only: unannotated collateralized submissions count as append-only.
                merging_data.or_else(|| {
                    config
                        .block_merging_config
                        .treat_as_append_only(submission.fee_recipient())
                        .then(|| BlockMergingData::append_only(submission.fee_recipient()))
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
        stats: &RefCell<DecodeStats>,
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
            mark_all_txs_mergeable: config.block_merging_config.mark_all_txs_mergeable,
            fork_name: chain_info.current_fork_name(),
        };

        let mut decoder = SubmissionDecoder::new(&decoder_params);
        let (mut submission, merging_data, bid_adjustment_data) =
            decoder.decode(payload, buffer)?;

        trace.decoded_ns = Nanos::now();
        record_submission_step_ns("recv_decoded", trace.receive_ns.0, trace.decoded_ns.0);

        let builder_pubkey = *submission.builder_pubkey();
        let skip_sigverify = if let Some(expected_pubkey) = expected_pubkey {
            if builder_pubkey != *expected_pubkey {
                return Err(BuilderApiError::InvalidBuilderPubkey(*expected_pubkey, builder_pubkey));
            }

            true
        } else {
            !header.api_key.is_empty() && cache.validate_api_key(&header.api_key, &builder_pubkey)
        };

        stats.borrow_mut().record_submission(
            builder_pubkey,
            header,
            merging_data.as_ref(),
            submission.num_txs(),
            skip_sigverify,
        );

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
