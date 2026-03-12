use std::sync::Arc;

use alloy_primitives::B256;
use flux::{
    spine::SpineProducers,
    tile::Tile,
    timing::{InternalMessage, Nanos},
};
use flux_utils::{DCache, SharedVector};
use helix_common::{
    RelayConfig, SubmissionTrace, chain_info::ChainInfo, local_cache::LocalCache,
    record_submission_step,
};
use helix_types::{
    BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, MergeableOrdersWithPref,
    SubmissionVersion,
};
use tracing::trace;

use crate::{
    HelixSpine,
    api::{
        FutureBidSubmissionResult,
        builder::{api::MAX_PAYLOAD_LENGTH, error::BuilderApiError},
    },
    auctioneer::{
        InternalBidSubmissionHeader, Submission, SubmissionData, SubmissionRef,
        get_mergeable_orders, send_submission_result,
    },
    bid_decoder::{
        SubmissionDataWithSpan,
        decoder::{
            DecodeFlags, SubmissionDecoder, decode_default, decode_dehydrated, decode_merge,
        },
    },
    spine::messages::{DecodedSubmission, NewBidSubmission},
};

pub struct DecoderTile {
    chain_info: ChainInfo,
    cache: LocalCache,
    config: RelayConfig,
    submissions: Arc<DCache>,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    buffer: Vec<u8>,
}

impl Tile<HelixSpine> for DecoderTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume_internal_message(
            |new_bid: &mut InternalMessage<NewBidSubmission>, producers| match self.submissions.map(
                new_bid.dref,
                |payload| {
                    let sent_at = new_bid.tracking_timestamp().publish_t();
                    Self::handle_block_submission(
                        &self.cache,
                        &self.chain_info,
                        &self.config,
                        &new_bid.submission_ref,
                        &new_bid.header,
                        payload,
                        &mut self.buffer,
                        new_bid.trace,
                        sent_at,
                        new_bid.expected_pubkey.as_ref(),
                    )
                },
            ) {
                Ok(Ok((submission, span))) => {
                    let ix = self.decoded.push(SubmissionDataWithSpan {
                        submission_data: submission,
                        span,
                        sent_at: new_bid.tracking_timestamp().publish_t(),
                    });
                    producers.produce(DecodedSubmission { ix });
                }
                Ok(Err(e)) => {
                    send_submission_result(
                        producers,
                        &self.future_results,
                        new_bid.submission_ref,
                        Err(e),
                    );
                }
                Err(e) => {
                    tracing::error!(%e, "dcache read failed");
                    send_submission_result(
                        producers,
                        &self.future_results,
                        new_bid.submission_ref,
                        Err(BuilderApiError::InternalError),
                    );
                }
            },
        );
    }
}

impl DecoderTile {
    pub fn new(
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
        submissions: Arc<DCache>,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    ) -> Self {
        Self {
            chain_info,
            cache,
            config,
            submissions,
            decoded,
            future_results,
            buffer: Vec::with_capacity(MAX_PAYLOAD_LENGTH),
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
        let (submission, withdrawals_root, version, merging_data, bid_adjustment_data) =
            Self::try_handle_block_submission(
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
            merging_data.and_then(|data| {
                if let Submission::Full(ref signed_bid_submission) = submission {
                    // TODO: split up mergeable order and submission processing to
                    // avoid delaying the bid update
                    match get_mergeable_orders(signed_bid_submission, &data) {
                        Ok(orders) => Some(MergeableOrdersWithPref {
                            allow_appending: data.allow_appending,
                            orders,
                        }),
                        Err(err) => {
                            tracing::error!(%err, "failed to process mergeable orders");
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
            submission_ref: *submission_ref,
            submission,
            version,
            merging_data,
            bid_adjustment_data,
            withdrawals_root,
            trace,
        };

        Ok((submission_data, tracing::Span::current()))
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
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
        (Submission, B256, SubmissionVersion, Option<BlockMergingData>, Option<BidAdjustmentData>),
        BuilderApiError,
    > {
        let mut decoder = SubmissionDecoder::new(header.compression, header.encoding);
        let body: &[u8] = match decoder.decompress(payload, buffer) {
            None => payload,
            Some(Ok(())) => buffer,
            Some(Err(e)) => return Err(e),
        };

        let with_mergeable_data = header.merge_type.is_some();
        let with_adjustments = header.flags.with_adjustments();
        let is_dehydrated = header.flags.is_dehydrated();

        let builder_pubkey = decoder.extract_builder_pubkey(body, with_mergeable_data)?;
        let skip_sigverify = if let Some(expected_pubkey) = expected_pubkey {
            if builder_pubkey != *expected_pubkey {
                return Err(BuilderApiError::InvalidBuilderPubkey(*expected_pubkey, builder_pubkey));
            }

            true
        } else {
            header.api_key.is_some_and(|api_key| cache.validate_api_key(&api_key, &builder_pubkey))
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
            block_merging_dry_run: config.block_merging_config.is_dry_run,
        };

        let (submission, merging_data, bid_adjustment_data) = if is_dehydrated {
            decode_dehydrated(&mut decoder, body, trace, chain_info, &flags)?
        } else if with_mergeable_data {
            decode_merge(&mut decoder, body, trace, chain_info, &flags)?
        } else {
            decode_default(&mut decoder, body, trace, chain_info, &flags)?
        };

        let withdrawals_root = submission.withdrawal_root();

        let version = SubmissionVersion::new(trace.receive_ns.0, header.sequence_number);
        Ok((submission, withdrawals_root, version, merging_data, bid_adjustment_data))
    }
}
