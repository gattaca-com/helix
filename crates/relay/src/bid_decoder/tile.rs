use std::sync::Arc;

use alloy_primitives::B256;
use flux::{
    spine::{DCacheRead, SpineProducers},
    tile::Tile,
    timing::{InternalMessage, Nanos},
};
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
    BidAdjustmentData, BlockMergingData, BlsPublicKeyBytes, MergeableOrdersWithPref,
    SignedBidSubmission, Submission, SubmissionVersion,
};
use tracing::trace;

use crate::{
    HelixSpine,
    api::{FutureBidSubmissionResult, builder::error::BuilderApiError},
    auctioneer::{
        InternalBidSubmissionHeader, SubmissionData, SubmissionRef, get_mergeable_orders,
        send_submission_result,
    },
    bid_decoder::SubmissionDataWithSpan,
    spine::messages::{DecodedSubmission, NewBidSubmission},
};

pub struct DecoderTile {
    chain_info: ChainInfo,
    cache: LocalCache,
    config: RelayConfig,
    decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    buffer: Vec<u8>,
    core: usize,
}

impl Tile<HelixSpine> for DecoderTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume_with_dcache_collaborative_internal_message(
            |new_bid: &InternalMessage<NewBidSubmission>, full_payload| {
                let payload = &full_payload[new_bid.payload_offset..];
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
            |res, producers| match res {
                DCacheRead::Ok((new_bid, result)) => match result {
                    Ok((submission, span)) => {
                        let ix = self.decoded.push(SubmissionDataWithSpan {
                            submission_data: submission,
                            span,
                            sent_at: new_bid.tracking_timestamp().publish_t(),
                        });
                        producers.produce(DecodedSubmission { ix });
                    }
                    Err(e) => {
                        send_submission_result(
                            producers,
                            &self.future_results,
                            new_bid.submission_ref,
                            Err(e),
                        );
                    }
                },
                DCacheRead::Lost(new_bid) | DCacheRead::NoRef(new_bid) => {
                    tracing::error!("dcache read failed");
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

    fn teardown(self, _adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {}

    fn name(&self) -> flux::tile::TileName {
        let mut name = flux_utils::short_typename::<Self>();
        name.push_str_truncate(self.core.to_string().as_str());
        name
    }
}

impl DecoderTile {
    pub fn new(
        cache: LocalCache,
        chain_info: ChainInfo,
        config: RelayConfig,
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        decoded: Arc<SharedVector<SubmissionDataWithSpan>>,
        core: usize,
    ) -> Self {
        Self {
            chain_info,
            cache,
            config,
            decoded,
            future_results,
            buffer: Vec::with_capacity(MAX_PAYLOAD_LENGTH),
            core,
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
            decoder_params,
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
            header.api_key.is_some_and(|api_key| cache.validate_api_key(&api_key, &builder_pubkey))
        };

        match submission {
            Submission::Full(ref mut signed_bid_submission) => {
                verify_and_validate(signed_bid_submission, skip_sigverify, chain_info)?;
            }
            Submission::Dehydrated { .. } => {
                if !skip_sigverify {
                    return Err(BuilderApiError::UntrustedBuilderOnDehydratedPayload);
                }
            }
        }

        let withdrawals_root = submission.withdrawal_root();

        trace!(
            ?header.sequence_number,
            is_dehydrated,
            skip_sigverify,
            with_mergeable_data,
            with_adjustments,
            "processed payload"
        );

        let version = SubmissionVersion::new(trace.receive_ns.0, header.sequence_number);
        Ok((
            submission,
            withdrawals_root,
            version,
            merging_data,
            bid_adjustment_data,
            decoder_params,
        ))
    }
}

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
