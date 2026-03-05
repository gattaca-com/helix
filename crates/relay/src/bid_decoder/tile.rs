use std::{sync::Arc, time::Instant};

use alloy_primitives::B256;
use flux::{spine::SpineProducers, tile::Tile};
use flux_utils::SharedVector;
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
    HelixSpine, InternalBidSubmission,
    api::{FutureBidSubmissionResult, builder::error::BuilderApiError},
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
    spine::messages::{DecodedSubmission, NewBidSubmissionIx},
};

pub struct DecoderTile {
    chain_info: ChainInfo,
    cache: LocalCache,
    config: RelayConfig,
    submissions: Arc<SharedVector<InternalBidSubmission>>,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
}

impl Tile<HelixSpine> for DecoderTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|new_bid: NewBidSubmissionIx, producers| {
            match self.submissions.get(new_bid.ix) {
                Some(bid) => match self.handle_bid(&bid) {
                    Ok(submission) => {
                        let ix = self.decoded.push(SubmissionDataWithSpan {
                            submission_data: submission,
                            span: bid.span.clone(),
                            sent_at: Instant::now(),
                        });
                        producers.produce(DecodedSubmission { ix });
                    }
                    Err(e) => {
                        send_submission_result(
                            producers,
                            &self.future_results,
                            bid.submission_ref,
                            Err(e),
                        );
                    }
                },
                None => {
                    tracing::error!(?new_bid, "No bid submission found");
                }
            }
        });
    }
}

impl DecoderTile {
    pub fn new(
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
        submissions: Arc<SharedVector<InternalBidSubmission>>,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    ) -> Self {
        Self { chain_info, cache, config, submissions, decoded, future_results }
    }

    fn handle_bid(
        &mut self,
        bid: &Arc<InternalBidSubmission>,
    ) -> Result<SubmissionData, BuilderApiError> {
        self.handle_block_submission(
            &bid.submission_ref,
            &bid.header,
            &bid.body,
            bid.trace,
            &bid.span,
            bid.sent_at,
            bid.expected_pubkey.as_ref(),
        )
    }

    fn handle_block_submission(
        &self,
        submission_ref: &SubmissionRef,
        header: &InternalBidSubmissionHeader,
        body: &bytes::Bytes,
        mut trace: SubmissionTrace,
        span: &tracing::Span,
        sent_at: Instant,
        expected_pubkey: Option<&BlsPublicKeyBytes>,
    ) -> Result<SubmissionData, BuilderApiError> {
        record_submission_step("worker_recv", sent_at.elapsed());
        let guard = span.enter();
        trace!("received by worker");
        let (submission, withdrawals_root, version, merging_data, bid_adjustment_data) =
            self.try_handle_block_submission(header, expected_pubkey, body, &mut trace)?;

        tracing::Span::current().record("bid_slot", tracing::field::display(submission.bid_slot()));
        tracing::Span::current()
            .record("block_hash", tracing::field::display(submission.block_hash()));
        tracing::Span::current()
            .record("builder_pubkey", tracing::field::display(submission.builder_pubkey()));

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

        Ok(submission_data)
    }

    #[allow(clippy::type_complexity)]
    fn try_handle_block_submission(
        &self,
        header: &InternalBidSubmissionHeader,
        expected_pubkey: Option<&BlsPublicKeyBytes>,
        body: &bytes::Bytes,
        trace: &mut SubmissionTrace,
    ) -> Result<
        (Submission, B256, SubmissionVersion, Option<BlockMergingData>, Option<BidAdjustmentData>),
        BuilderApiError,
    > {
        let mut decoder = SubmissionDecoder::new(header.compression, header.encoding);
        let body = match decoder.decompress(body) {
            None => body,
            Some(res) => &res?,
        };

        let with_mergeable_data = header.merge_type.is_some();
        let with_adjustments = header.flags.with_adjustments();
        let is_dehydrated = header.flags.is_dehydrated();

        let builder_pubkey = decoder.extract_builder_pubkey(body.as_ref(), with_mergeable_data)?;
        let skip_sigverify = if let Some(expected_pubkey) = expected_pubkey {
            if builder_pubkey != *expected_pubkey {
                return Err(BuilderApiError::InvalidBuilderPubkey(*expected_pubkey, builder_pubkey));
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

        let version = SubmissionVersion::new(trace.receive_ns.0, header.sequence_number);
        Ok((submission, withdrawals_root, version, merging_data, bid_adjustment_data))
    }
}
