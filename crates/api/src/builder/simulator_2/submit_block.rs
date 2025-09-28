use std::{
    collections::BTreeMap,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{B256, U256};
use helix_beacon::{multi_beacon_client::MultiBeaconClient, types::BroadcastValidation};
use helix_common::{
    api::builder_api::{
        BuilderGetValidatorsResponseEntry, InclusionListWithKey, InclusionListWithMetadata,
    },
    bid_submission::{BidSubmission, OptimisticVersion},
    chain_info::ChainInfo,
    metrics::{SimulatorMetrics, HYDRATION_CACHE_HITS},
    simulator::BlockSimError,
    utils::utcnow_ns,
    BuilderInfo, GetPayloadTrace, RelayConfig, SubmissionTrace,
};
use helix_database::DatabaseService;
use helix_housekeeper::PayloadAttributesUpdate;
use helix_types::{
    BlsPublicKeyBytes, BuilderBid, DehydratedBidSubmission, ForkName, GetPayloadResponse,
    HydrationCache, SignedBidSubmission, SignedBlindedBeaconBlock, SignedBuilderBid, Slot,
    VersionedSignedProposal,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::{
    builder::{
        error::BuilderApiError,
        simulator_2::{
            bid_sorter::{BidSorter, BidSorterMessage},
            sim_manager::{SimulationResult, SimulatorManager},
            worker::{GetPayloadResult, GetPayloadResultData, Submission, SubmissionResult},
            Context, SortingData,
        },
        BlockSimRequest, SimReponse, SimulatorRequest,
    },
    gossiper::{grpc_gossiper::GrpcGossiperClientManager, types::BroadcastGetPayloadParams},
    proposer::{GetHeaderParams, ProposerApiError},
    Api,
};

impl SortingData {
    pub(super) fn handle_submission<A: Api>(
        &mut self,
        submission: Submission,
        withdrawals_root: B256,
        sequence: Option<u64>,
        mut trace: SubmissionTrace,
        ctx: &mut Context<A>,
        res_tx: oneshot::Sender<SubmissionResult>,
    ) {
        match self.validate_and_sort_submission(
            submission,
            withdrawals_root,
            sequence,
            &mut trace,
            ctx,
        ) {
            Ok((submission, optimistic_version)) => {
                let res_tx = if optimistic_version.is_optimistic() {
                    let _ = res_tx.send(Ok(()));
                    None
                } else {
                    Some(res_tx)
                };
                self.handle_2(submission, trace, optimistic_version, ctx, res_tx);
            }

            Err(err) => {
                let _ = res_tx.send(Err(err));
            }
        }
    }

    pub(super) fn handle_simulated_submission(&mut self, result: &SimulationResult) {
        match &result.result {
            Err(err) if err.is_demotable() => {
                self.sort.demote(*result.submission.builder_public_key());
            }

            Ok(_) | Err(_) => {
                if !result.was_optimistic_sim() {
                    // TODO: tidy this up, we store the submission not the header
                    let msg = BidSorterMessage::new_from_block_submission(
                        &result.submission,
                        &Default::default(),
                        utcnow_ns(),
                        100,
                        result.submission.withdrawals_root(),
                    );

                    self.sort.sort(msg);
                }
            }
        }
    }

    fn validate_and_sort_submission<A: Api>(
        &mut self,
        submission: Submission,
        withdrawals_root: B256,
        sequence: Option<u64>,
        trace: &mut SubmissionTrace,
        ctx: &Context<A>,
    ) -> Result<(SignedBidSubmission, OptimisticVersion), BuilderApiError> {
        let submission = match submission {
            Submission::Full(full) => full,
            Submission::Dehydrated(dehydrated) => {
                let (payload, tx_cache_hits, blob_cache_hits) =
                    dehydrated.hydrate(&mut self.hydration_cache)?;

                HYDRATION_CACHE_HITS
                    .with_label_values(&["transaction"])
                    .inc_by(tx_cache_hits as u64);
                HYDRATION_CACHE_HITS.with_label_values(&["blob"]).inc_by(blob_cache_hits as u64);

                payload.validate_payload_ssz_lengths()?;
                payload
            }
        };

        let builder_info = ctx
            .builder_info
            .get(submission.builder_public_key())
            .unwrap_or(&ctx.unknown_builder_info);

        self.validate_submission(&submission, &withdrawals_root, sequence, builder_info)?;

        let optimistic_version = if ctx.can_process_optimistic &&
            self.should_process_optimistically(&submission, builder_info)
        {
            // TODO: tidy this up, we store the submission not the header
            let msg = BidSorterMessage::new_from_block_submission(
                &submission,
                trace,
                utcnow_ns(),
                0,
                withdrawals_root,
            );
            self.sort.sort(msg);
            OptimisticVersion::V1
        } else {
            OptimisticVersion::NotOptimistic
        };

        Ok((submission, optimistic_version))
    }

    fn handle_2<A: Api>(
        &mut self,
        submission: SignedBidSubmission,
        submission_trace: SubmissionTrace,
        optimistic_version: OptimisticVersion,
        ctx: &mut Context<A>,
        res_tx: Option<oneshot::Sender<SubmissionResult>>,
    ) {
        // TODO: pass this from previous step
        let is_top_bid = self.sort.is_top_bid(&submission);
        let inclusion_list = self.inclusion_list.clone();

        let request = BlockSimRequest::new(
            self.slot.registration_data.entry.registration.message.gas_limit,
            &submission,
            self.slot.registration_data.entry.preferences.clone(),
            self.slot.payload_attributes.payload_attributes.parent_beacon_block_root,
            inclusion_list,
        );

        // send to sim_manager queue, use another oneshot or simply the sim results?
        let req = SimulatorRequest {
            request,
            on_receive_ns: submission_trace.receive,
            is_top_bid,
            is_optimistic: optimistic_version.is_optimistic(),
            res_tx,
            submission: submission.clone(),
        };
        ctx.simulator_manager.handle_sim_request(req);

        let payload_and_blobs = Arc::new(submission.payload_and_blobs_ref().to_owned());
        self.payloads.insert(*submission.block_hash(), payload_and_blobs);

        let db = ctx.db.clone();
        tokio::spawn(async move {
            if let Err(err) =
                db.store_block_submission(submission, submission_trace, optimistic_version).await
            {
                error!(%err, "failed to store block submission")
            }
        });
    }

    fn should_process_optimistically(
        &self,
        submission: &SignedBidSubmission,
        builder_info: &BuilderInfo,
    ) -> bool {
        if builder_info.is_optimistic && submission.message().value <= builder_info.collateral {
            if self.slot.registration_data.entry.preferences.filtering.is_regional() &&
                !builder_info.can_process_regional_slot_optimistically()
            {
                return false;
            }

            return true;
        }

        false
    }
}
