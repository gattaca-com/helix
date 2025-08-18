use std::{io::Read, sync::Arc, time::Duration};

use alloy_consensus::{TxEip4844, TxType};
use alloy_primitives::{B256, U256};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::IntoResponse,
    Extension,
};
use bytes::Bytes;
use dashmap::DashMap;
use flate2::read::GzDecoder;
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    bid_sorter::{BestGetHeader, BidSorterMessage, FloorBid},
    bid_submission::BidSubmission,
    chain_info::ChainInfo,
    merging_pool::MergingPoolMessage,
    simulator::BlockSimError,
    utils::{get_slot_coordinate, utcnow_ns},
    BuilderInfo, RelayConfig, SubmissionTrace, ValidatorPreferences,
};
use helix_database::DatabaseService;
use helix_datastore::{redis::redis_cache::InclusionListWithKey, Auctioneer};
use helix_housekeeper::{CurrentSlotInfo, PayloadAttributesUpdate};
use helix_types::{
    BlobsBundle, BlockMergingData, BlsPublicKey, MergeableBundle, MergeableOrder, MergeableOrders,
    MergeableTransaction, Order, SignedBidSubmission, SignedBidSubmissionWithMergingData, Slot,
    Transactions, TxIndices,
};
use parking_lot::RwLock;
use tracing::{debug, error, trace, warn};

use super::multi_simulator::MultiSimulator;
use crate::{
    builder::{error::BuilderApiError, v2_check::V2SubMessage, BlockSimRequest},
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    Api,
};

pub(crate) const MAX_PAYLOAD_LENGTH: usize = 1024 * 1024 * 10;

#[derive(Clone)]
pub struct BuilderApi<A: Api> {
    pub auctioneer: Arc<A::Auctioneer>,
    pub db: Arc<A::DatabaseService>,
    pub chain_info: Arc<ChainInfo>,
    pub simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub metadata_provider: Arc<A::MetadataProvider>,
    pub relay_config: Arc<RelayConfig>,
    pub curr_slot_info: CurrentSlotInfo,
    pub _validator_preferences: Arc<ValidatorPreferences>,
    pub current_inclusion_list: Arc<RwLock<Option<InclusionListWithKey>>>,
    /// Send blocks to the bid sorter
    pub sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
    /// Send mergeable orders to the merging pool
    pub pool_tx: crossbeam_channel::Sender<MergingPoolMessage>,
    /// Subscriber for TopBid updates, SSZ encoded
    pub top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    /// Send headers/blocks to be checked for V2 submissions
    pub v2_checks_tx: tokio::sync::mpsc::Sender<V2SubMessage>,
    /// Set in sorter loop
    pub shared_floor: FloorBid,
    /// Cache of tx roots for v2 submissions
    pub tx_root_cache: DashMap<B256, (u64, B256)>,
    /// Best get header to check the current top bid on simulations
    pub shared_best_header: BestGetHeader,
}

impl<A: Api> BuilderApi<A> {
    pub fn new(
        auctioneer: Arc<A::Auctioneer>,
        db: Arc<A::DatabaseService>,
        chain_info: Arc<ChainInfo>,
        simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        metadata_provider: Arc<A::MetadataProvider>,
        relay_config: RelayConfig,
        validator_preferences: Arc<ValidatorPreferences>,
        curr_slot_info: CurrentSlotInfo,
        sorter_tx: crossbeam_channel::Sender<BidSorterMessage>,
        pool_tx: crossbeam_channel::Sender<MergingPoolMessage>,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        v2_checks_tx: tokio::sync::mpsc::Sender<V2SubMessage>,
        shared_floor: FloorBid,
        shared_best_header: BestGetHeader,
    ) -> Self {
        let tx_root_cache = DashMap::with_capacity(1000);

        let cache = tx_root_cache.clone();
        let info = chain_info.clone();
        tokio::spawn(async move {
            // cleanup cache, keep only last 2 slots worth of roots
            let mut last_cleared_slot = 0;

            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                let curr_slot = info.current_slot().as_u64();

                if curr_slot > last_cleared_slot {
                    last_cleared_slot = curr_slot;
                    cache.retain(|_, (slot, _)| curr_slot.saturating_sub(*slot) <= 2);
                }
            }
        });

        Self {
            auctioneer,
            db,
            chain_info,
            simulator,
            gossiper,
            metadata_provider,
            relay_config: Arc::new(relay_config),

            curr_slot_info,
            _validator_preferences: validator_preferences,
            current_inclusion_list: Default::default(),

            sorter_tx,
            pool_tx,
            top_bid_tx,
            v2_checks_tx,
            shared_floor,

            tx_root_cache,
            shared_best_header,
        }
    }

    /// Implements this API: <https://flashbots.github.io/relay-specs/#/Builder/getValidators>
    pub async fn get_validators(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
    ) -> impl IntoResponse {
        if let Some(duty_bytes) = api.curr_slot_info.proposer_duties_response() {
            (StatusCode::OK, duty_bytes.0).into_response()
        } else {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }

    pub(crate) fn fetch_payload_attributes(
        &self,
        slot: Slot,
        parent_hash: B256,
        block_hash: &B256,
    ) -> Result<PayloadAttributesUpdate, BuilderApiError> {
        let Some(payload_attributes) = self.curr_slot_info.payload_attributes(parent_hash, slot)
        else {
            warn!(%block_hash, "payload attributes not yet known");
            return Err(BuilderApiError::PayloadAttributesNotYetKnown);
        };

        if payload_attributes.slot != slot {
            warn!(
                got =% slot,
                expected =% payload_attributes.slot,
                "payload attributes slot mismatch with payload attributes"
            );
            return Err(BuilderApiError::PayloadSlotMismatchWithPayloadAttributes {
                got: slot,
                expected: payload_attributes.slot,
            });
        }

        Ok(payload_attributes)
    }

    /// Check for block hashes that have already been processed.
    /// If this is the first time the hash has been seen it will insert the hash into the set.
    ///
    /// This function should not be called by functions that only process the payload.
    pub(crate) async fn check_for_duplicate_block_hash(
        &self,
        block_hash: &B256,
    ) -> Result<(), BuilderApiError> {
        match self.auctioneer.seen_or_insert_block_hash(block_hash).await {
            Ok(false) => Ok(()),
            Ok(true) => {
                debug!(?block_hash, "duplicate block hash");
                Err(BuilderApiError::DuplicateBlockHash { block_hash: *block_hash })
            }
            Err(err) => {
                error!(%err, "failed to call seen_or_insert_block_hash");
                Err(BuilderApiError::InternalError)
            }
        }
    }

    /// This function verifies:
    /// 1. Verifies the payload signature.
    /// 2. Simulates the submission
    ///
    /// Returns: the bid submission in an Arc.
    pub(crate) async fn verify_submitted_block(
        &self,
        payload: &SignedBidSubmission,
        next_duty: BuilderGetValidatorsResponseEntry,
        builder_info: &BuilderInfo,
        trace: &mut SubmissionTrace,
        payload_attributes: &PayloadAttributesUpdate,
    ) -> Result<bool, BuilderApiError> {
        // Verify the payload signature
        if let Err(err) = payload.verify_signature(&self.chain_info.context) {
            warn!(%err, "failed to verify signature");
            return Err(BuilderApiError::SignatureVerificationFailed);
        }
        trace!("verified signature");
        trace.signature = utcnow_ns();

        let curr_best = self.shared_best_header.best_bid(payload.slot().as_u64());
        let is_top_bid = payload.value() > curr_best;

        // Simulate the submission
        let was_simulated_optimistically = self
            .simulate_submission(
                payload,
                builder_info,
                trace,
                next_duty.entry,
                payload_attributes,
                is_top_bid,
            )
            .await?;

        Ok(was_simulated_optimistically)
    }

    /// If the proposer has specified a list of trusted builders ensure
    /// that the submitting builder pubkey is in that list.
    /// Verifies that if the proposer has specified a list of trusted builders,
    /// the builder submitting a request is in that list.
    ///
    /// The auctioneer maintains a mapping of builder public keys to corresponding IDs.
    /// This function retrieves the ID associated with the builder's public key from the auctioneer.
    /// It then checks if this ID is included in the list of trusted builders specified by the
    /// proposer.
    pub(crate) fn check_if_trusted_builder(
        next_duty: &BuilderGetValidatorsResponseEntry,
        builder_info: &BuilderInfo,
    ) -> bool {
        if let Some(trusted_builders) = &next_duty.entry.preferences.trusted_builders {
            // Handle case where proposer specifies an empty list.
            if trusted_builders.is_empty() {
                return true;
            }

            if let Some(builder_id) = &builder_info.builder_id {
                trusted_builders.contains(builder_id)
            } else if let Some(ids) = &builder_info.builder_ids {
                ids.iter().any(|id| trusted_builders.contains(id))
            } else {
                false
            }
        } else {
            true
        }
    }

    /// Simulates a new block payload.
    ///
    /// 1. Checks the current top bid value from the auctioneer.
    /// 3. Invokes the block simulator for validation.
    async fn simulate_submission(
        &self,
        payload: &SignedBidSubmission,
        builder_info: &BuilderInfo,
        trace: &mut SubmissionTrace,
        registration_info: ValidatorRegistrationInfo,
        payload_attributes: &PayloadAttributesUpdate,
        is_top_bid: bool,
    ) -> Result<bool, BuilderApiError> {
        debug!("validating block");

        let current_slot_coord = get_slot_coordinate(
            payload.slot().as_u64(),
            payload.proposer_public_key(),
            payload.parent_hash(),
        );

        let inclusion_list = self
            .current_inclusion_list
            .read()
            .as_ref()
            .filter(|il| il.key == current_slot_coord)
            .map(|il| il.inclusion_list.clone());

        let sim_request = BlockSimRequest::new(
            registration_info.registration.message.gas_limit,
            payload,
            registration_info.preferences,
            payload_attributes.payload_attributes.parent_beacon_block_root,
            inclusion_list,
        );

        let result = self.simulator.process_request(sim_request, builder_info, is_top_bid).await;

        match result {
            Ok(sim_optimistic) => {
                trace.simulation = utcnow_ns();
                debug!(
                    sim_latency = trace.simulation.saturating_sub(trace.signature),
                    "block simulation successful"
                );

                Ok(sim_optimistic)
            }
            Err(err) => match &err {
                BlockSimError::BlockValidationFailed(reason) => {
                    warn!(err = %reason, "block validation failed");
                    Err(BuilderApiError::BlockValidationError(err))
                }
                _ => {
                    error!(%err, "error simulating block");
                    Err(BuilderApiError::InternalError)
                }
            },
        }
    }

    /// Checks if the builder has enough collateral to submit an optimistic bid.
    /// Or if the builder is not optimistic.
    ///
    /// This function compares the builder's collateral with the block value for a bid submission.
    /// If the builder's collateral is less than the required value, it returns an error.
    pub(crate) fn check_builder_collateral(
        payload: &impl BidSubmission,
        builder_info: &BuilderInfo,
    ) -> Result<(), BuilderApiError> {
        if !builder_info.is_optimistic {
            warn!(
                builder=%payload.builder_public_key(),
                "builder is not optimistic"
            );
            return Err(BuilderApiError::BuilderNotOptimistic {
                builder_pub_key: payload.builder_public_key().clone(),
            });
        } else if builder_info.collateral < payload.value() {
            warn!(
                builder=?payload.builder_public_key(),
                collateral=%builder_info.collateral,
                collateral_required=%payload.value(),
                "builder does not have enough collateral"
            );
            return Err(BuilderApiError::NotEnoughOptimisticCollateral {
                builder_pub_key: payload.builder_public_key().clone().into(),
                collateral: builder_info.collateral,
                collateral_required: payload.value(),
                is_optimistic: builder_info.is_optimistic,
            });
        }

        // Builder has enough collateral
        Ok(())
    }

    /// Fetch the builder's information. Default info is returned if fetching fails.
    pub(crate) async fn fetch_builder_info(&self, builder_pub_key: &BlsPublicKey) -> BuilderInfo {
        match self.auctioneer.get_builder_info(builder_pub_key).await {
            Ok(info) => info,
            Err(err) => {
                warn!(
                    builder=?builder_pub_key,
                    err=%err,
                    "Failed to retrieve builder info"
                );
                BuilderInfo {
                    collateral: U256::ZERO,
                    is_optimistic: false,
                    is_optimistic_for_regional_filtering: false,
                    builder_id: None,
                    builder_ids: None,
                }
            }
        }
    }

    pub(crate) async fn demote_builder(
        &self,
        slot: u64,
        builder: &BlsPublicKey,
        block_hash: &B256,
        err: &BuilderApiError,
    ) {
        if let BuilderApiError::BlockValidationError(sim_err) = err {
            if sim_err.is_temporary() {
                return;
            }
        }

        error!(%err, %builder, "verification failed for submit_block_v2. Demoting builder!");

        if let Err(err) = self.auctioneer.demote_builder(builder).await {
            error!(%err, %builder, "failed to demote builder in auctioneer");
        }

        if let Err(err) =
            self.db.db_demote_builder(slot, builder, block_hash, err.to_string()).await
        {
            error!(%err,  %builder, "Failed to demote builder in database");
        }
    }

    pub(crate) fn get_current_floor(&self, bid_slot: Slot) -> U256 {
        self.shared_floor.get(bid_slot.as_u64())
    }
}

/// `decode_payload` decodes the payload into a `SignedBidSubmission` object.
///
/// - Supports both SSZ and JSON encodings for deserialization.
/// - Automatically falls back to JSON if SSZ deserialization fails.
/// - Handles GZIP-compressed payloads.
///
/// It returns a tuple of the decoded payload and if cancellations are enabled.
#[tracing::instrument(skip_all)]
pub async fn decode_payload(
    req: Request<Body>,
    trace: &mut SubmissionTrace,
    has_mergeable_data: bool,
) -> Result<(SignedBidSubmissionWithMergingData, bool), BuilderApiError> {
    // Extract the query parameters
    let is_cancellations_enabled = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|part| {
            let mut split = part.splitn(2, '=');
            if split.next()? == "cancellations" {
                Some(split.next()? == "1")
            } else {
                None
            }
        })
        .unwrap_or(false);

    // Get content encoding and content type
    let is_gzip =
        req.headers().get("Content-Encoding").and_then(|val| val.to_str().ok()) == Some("gzip");

    let is_ssz = req.headers().get("Content-Type").and_then(|val| val.to_str().ok()) ==
        Some("application/octet-stream");

    // Read the body
    let body = req.into_body();
    trace!("reading body");
    let mut body_bytes = to_bytes(body, MAX_PAYLOAD_LENGTH).await?;
    if body_bytes.len() > MAX_PAYLOAD_LENGTH {
        return Err(BuilderApiError::PayloadTooLarge {
            max_size: MAX_PAYLOAD_LENGTH,
            size: body_bytes.len(),
        });
    }
    trace!("read body");
    trace.read_body = utcnow_ns();

    let size_compressed = body_bytes.len();
    // Decompress if necessary
    if is_gzip {
        let mut decoder = GzDecoder::new(&body_bytes[..]);

        // TODO: profile this. 2 is a guess.
        let estimated_size = body_bytes.len() * 2;
        let mut buf = Vec::with_capacity(estimated_size);

        decoder.read_to_end(&mut buf)?;
        body_bytes = buf.into();
    }

    trace!(size_compressed, size_uncompressed = body_bytes.len(), is_gzip, "decompressed payload");

    // Decode payload
    let payload: SignedBidSubmissionWithMergingData = if has_mergeable_data {
        decode_submission(&body_bytes, is_ssz)?
    } else {
        let submission = decode_submission(&body_bytes, is_ssz)?;
        SignedBidSubmissionWithMergingData { submission, merging_data: Default::default() }
    };

    trace.decode = utcnow_ns();
    debug!(
        timestamp_after_decoding = trace.decode,
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        builder_pub_key = ?payload.submission.builder_public_key(),
        block_hash = ?payload.submission.block_hash(),
        proposer_pubkey = ?payload.submission.proposer_public_key(),
        parent_hash = ?payload.submission.parent_hash(),
        value = ?payload.submission.value(),
        num_tx = payload.submission.execution_payload_ref().transactions().len(),
        "payload info"
    );

    Ok((payload, is_cancellations_enabled))
}

fn decode_submission<'a, T>(body_bytes: &'a Bytes, is_ssz: bool) -> Result<T, BuilderApiError>
where
    T: ssz::Decode + serde::Deserialize<'a>,
{
    if is_ssz {
        match T::from_ssz_bytes(body_bytes) {
            Ok(payload) => Ok(payload),
            Err(err) => {
                // Fallback to JSON
                warn!(?err, "failed to decode payload using SSZ; falling back to JSON");
                Ok(serde_json::from_slice(body_bytes)?)
            }
        }
    } else {
        Ok(serde_json::from_slice(body_bytes)?)
    }
}

/// - Validates the expected block.timestamp.
/// - Ensures that the fee recipients in the payload and proposer duty match.
/// - Ensures that the slot in the payload and payload attributes match.
/// - Validates that the block hash in the payload and message are the same.
/// - Validates that the parent hash in the payload and message are the same.
pub(crate) fn sanity_check_block_submission(
    payload: &impl BidSubmission,
    next_duty: &BuilderGetValidatorsResponseEntry,
    payload_attributes: &PayloadAttributesUpdate,
    chain_info: &ChainInfo,
) -> Result<(), BuilderApiError> {
    // Check block is for current fork
    if chain_info.current_fork_name() != payload.fork_name() {
        return Err(BuilderApiError::InvalidPayloadType {
            fork_name: chain_info.current_fork_name().to_string(),
        });
    }

    // checks internal consistency of the payload
    payload.validate()?;

    let bid_trace = payload.bid_trace();

    let expected_timestamp =
        chain_info.genesis_time_in_secs + (bid_trace.slot * chain_info.seconds_per_slot());
    if payload.timestamp() != expected_timestamp {
        return Err(BuilderApiError::IncorrectTimestamp {
            got: payload.timestamp(),
            expected: expected_timestamp,
        });
    }

    // Check duty
    if next_duty.entry.registration.message.fee_recipient != *payload.proposer_fee_recipient() {
        return Err(BuilderApiError::FeeRecipientMismatch {
            got: *payload.proposer_fee_recipient(),
            expected: next_duty.entry.registration.message.fee_recipient,
        });
    }

    if payload.slot() != next_duty.slot {
        return Err(BuilderApiError::SlotMismatch {
            got: payload.slot().into(),
            expected: next_duty.slot.into(),
        });
    }

    if next_duty.entry.registration.message.pubkey != bid_trace.proposer_pubkey {
        return Err(BuilderApiError::ProposerPublicKeyMismatch {
            got: bid_trace.proposer_pubkey.clone().into(),
            expected: next_duty.entry.registration.message.pubkey.clone().into(),
        });
    }

    // Check payload attrs
    if *payload.prev_randao() != payload_attributes.payload_attributes.prev_randao {
        return Err(BuilderApiError::PrevRandaoMismatch {
            got: *payload.prev_randao(),
            expected: payload_attributes.payload_attributes.prev_randao,
        });
    }

    let withdrawals_root = payload.withdrawals_root();

    let expected_withdrawals_root = payload_attributes.withdrawals_root;

    if withdrawals_root != expected_withdrawals_root {
        return Err(BuilderApiError::WithdrawalsRootMismatch {
            got: withdrawals_root,
            expected: expected_withdrawals_root,
        });
    }

    Ok(())
}

/// Expands the references in [`BlockMergingData`] from the transactions in the
/// payload of the given submission. If any bundle references a transaction not in
/// the payload, it will be silently ignored.
pub fn get_mergeable_orders(
    payload: &SignedBidSubmission,
    merging_data: &BlockMergingData,
) -> Result<MergeableOrders, &'static str> {
    let execution_payload = payload.execution_payload_ref();
    let block_blobs_bundles = payload.blobs_bundle();
    let blob_versioned_hashes: Vec<_> =
        block_blobs_bundles.commitments.iter().map(|c| c.calculate_versioned_hash()).collect();
    let txs = execution_payload.transactions();
    let mergeable_orders = merging_data
        .merge_orders
        .iter()
        .map(|order| order_to_mergeable(order, &txs, &blob_versioned_hashes, &block_blobs_bundles))
        .collect::<Result<_, &'static str>>()?;

    Ok(MergeableOrders::new(merging_data.builder_address, mergeable_orders))
}

fn order_to_mergeable(
    order: &Order,
    txs: &Transactions,
    blob_versioned_hashes: &[B256],
    block_blobs_bundles: &BlobsBundle,
) -> Result<MergeableOrder, &'static str> {
    match order {
        Order::Tx(tx) => {
            let Some(raw_tx) = txs.get(tx.index) else {
                debug!(
                    "Got invalid block merging index {}, with a tx count of {}",
                    tx.index,
                    txs.len()
                );
                return Err("invalid block merging tx index");
            };
            let mut blobs_bundle = None;
            if is_blob_transaction(raw_tx) {
                // If the tx references bundles not in the block, we drop it
                let bundle = get_blobs_bundle_from_blob_transaction(
                    raw_tx,
                    &blob_versioned_hashes,
                    &block_blobs_bundles,
                )?;
                blobs_bundle = Some(bundle);
            }

            Ok(MergeableTransaction {
                transaction: Bytes::from(raw_tx.to_vec()),
                can_revert: tx.can_revert,
                blobs_bundle,
            }
            .into())
        }
        Order::Bundle(bundle) => {
            let mut blobs_bundle: Option<BlobsBundle> = None;
            let transactions = bundle
                .txs
                .iter()
                .map(|tx_index| {
                    let Some(raw_tx) = txs.get(*tx_index) else {
                        debug!(
                            "Got invalid block merging index {} in bundle, with a tx count of {}",
                            tx_index,
                            txs.len()
                        );
                        return Err("invalid block merging bundle index");
                    };

                    if is_blob_transaction(raw_tx) {
                        // If the tx references bundles not in the block, we drop the bundle
                        let bundle = get_blobs_bundle_from_blob_transaction(
                            raw_tx,
                            &blob_versioned_hashes,
                            &block_blobs_bundles,
                        )?;
                        // Add blobs to current bundle
                        if let Some(existing_bundle) = &mut blobs_bundle {
                            // If number of blobs goes over limit, we skip the bundle
                            extend_bundle(existing_bundle, bundle)?;
                        } else {
                            blobs_bundle = Some(bundle);
                        }
                    }

                    Ok(Bytes::from_owner(raw_tx.to_vec()))
                })
                .collect::<Result<_, &'static str>>()?;

            let reverting_txs = update_flagged_indices(&bundle.txs, &bundle.reverting_txs)?;
            let dropping_txs = update_flagged_indices(&bundle.txs, &bundle.dropping_txs)?;

            Ok(MergeableBundle { transactions, reverting_txs, dropping_txs, blobs_bundle }.into())
        }
    }
}

fn extend_bundle(bundle: &mut BlobsBundle, other_bundle: BlobsBundle) -> Result<(), &'static str> {
    other_bundle
        .commitments
        .into_iter()
        .map(|c| bundle.commitments.push(c))
        .collect::<Result<(), _>>()
        .map_err(|_| "reached commitments limit")?;
    other_bundle
        .proofs
        .into_iter()
        .map(|c| bundle.proofs.push(c))
        .collect::<Result<(), _>>()
        .map_err(|_| "reached proofs limit")?;
    other_bundle
        .blobs
        .into_iter()
        .map(|c| bundle.blobs.push(c))
        .collect::<Result<(), _>>()
        .map_err(|_| "reached blobs limit")?;

    Ok(())
}

fn update_flagged_indices(
    tx_indices: &[usize],
    flagged_indices: &[usize],
) -> Result<TxIndices, &'static str> {
    let new_indices: TxIndices = tx_indices
        .iter()
        .enumerate()
        .filter(|(_, tx_index)| flagged_indices.contains(tx_index))
        .map(|(i, _)| i)
        .collect();
    if new_indices.len() != flagged_indices.len() {
        return Err("flagged indices reference tx outside of bundle");
    }
    Ok(new_indices)
}

fn is_blob_transaction(raw_tx: &[u8]) -> bool {
    raw_tx.first().is_some_and(|&b| b == TxType::Eip4844)
}

fn get_tx_versioned_hashes(mut raw_tx: &[u8]) -> Vec<B256> {
    use alloy_consensus::transaction::RlpEcdsaDecodableTx;
    TxEip4844::rlp_decode_with_signature(&mut raw_tx)
        .map(|(b, _)| b.blob_versioned_hashes)
        .unwrap_or(vec![])
}

fn get_blobs_bundle_from_blob_transaction(
    raw_tx: &[u8],
    blob_versioned_hashes: &[B256],
    block_blobs_bundles: &BlobsBundle,
) -> Result<BlobsBundle, &'static str> {
    let versioned_hashes = get_tx_versioned_hashes(raw_tx);
    let num_blobs = versioned_hashes.len();
    if num_blobs == 0 {
        return Err("blob transaction does not reference any blobs");
    }
    let (commitments, (proofs, blobs)): (Vec<_>, (Vec<_>, Vec<_>)) = versioned_hashes
        .into_iter()
        .map(|h| {
            let index = blob_versioned_hashes.iter().position(|vh| *vh == h)?;
            let commitment = block_blobs_bundles.commitments[index].clone();
            let proof = block_blobs_bundles.proofs[index].clone();
            let blob = block_blobs_bundles.blobs[index].clone();
            Some((commitment, (proof, blob)))
        })
        .flatten()
        .unzip();
    if commitments.len() != num_blobs {
        return Err("blob transaction references blobs not in the block");
    }
    let bundle =
        BlobsBundle { commitments: commitments.into(), proofs: proofs.into(), blobs: blobs.into() };
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use axum::http::{
        header::{CONTENT_ENCODING, CONTENT_TYPE},
        HeaderValue, Uri,
    };
    use ssz::Decode;

    use super::*;

    async fn build_test_request(payload: Vec<u8>, is_gzip: bool, is_ssz: bool) -> Request<Body> {
        let mut req = Request::new(Body::from(payload));
        *req.uri_mut() = Uri::from_static("/some_path?cancellations=1");

        if is_gzip {
            req.headers_mut().insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }

        if is_ssz {
            req.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        }

        req
    }

    async fn create_test_submission_trace() -> SubmissionTrace {
        SubmissionTrace::default()
    }

    #[tokio::test]
    async fn test_decode_json_payload() {
        let json_payload: Vec<u8> = vec![];

        match serde_json::from_slice::<SignedBidSubmission>(&json_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_empty_tx_payload_json() {
        let json_payload = vec![
            123, 10, 32, 32, 34, 109, 101, 115, 115, 97, 103, 101, 34, 58, 32, 123, 10, 32, 32, 32,
            32, 34, 115, 108, 111, 116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 112,
            97, 114, 101, 110, 116, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48, 120, 99, 102, 56,
            101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57,
            48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56,
            56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44,
            10, 32, 32, 32, 32, 34, 98, 108, 111, 99, 107, 95, 104, 97, 115, 104, 34, 58, 32, 34,
            48, 120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51,
            48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52,
            51, 100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57,
            50, 48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 98, 117, 105, 108, 100, 101, 114, 95,
            112, 117, 98, 107, 101, 121, 34, 58, 32, 34, 48, 120, 57, 51, 50, 52, 55, 102, 50, 50,
            48, 57, 97, 98, 99, 97, 99, 102, 53, 55, 98, 55, 53, 97, 53, 49, 100, 97, 102, 97, 101,
            55, 55, 55, 102, 57, 100, 100, 51, 56, 98, 99, 55, 48, 53, 51, 100, 49, 97, 102, 53,
            50, 54, 102, 50, 50, 48, 97, 55, 52, 56, 57, 97, 54, 100, 51, 97, 50, 55, 53, 51, 101,
            53, 102, 51, 101, 56, 98, 49, 99, 102, 101, 51, 57, 98, 53, 54, 102, 52, 51, 54, 49,
            49, 100, 102, 55, 52, 97, 34, 44, 10, 32, 32, 32, 32, 34, 112, 114, 111, 112, 111, 115,
            101, 114, 95, 112, 117, 98, 107, 101, 121, 34, 58, 32, 34, 48, 120, 56, 53, 53, 57, 55,
            50, 55, 101, 101, 54, 53, 99, 50, 57, 53, 50, 55, 57, 51, 51, 50, 49, 57, 56, 48, 50,
            57, 99, 57, 51, 57, 53, 53, 55, 102, 52, 100, 50, 97, 98, 97, 48, 55, 53, 49, 102, 99,
            53, 53, 102, 55, 49, 100, 48, 55, 51, 51, 98, 56, 97, 97, 49, 55, 99, 100, 48, 51, 48,
            49, 50, 51, 50, 97, 55, 102, 50, 49, 97, 56, 57, 53, 102, 56, 49, 101, 97, 99, 102, 53,
            53, 99, 57, 55, 101, 99, 52, 34, 44, 10, 32, 32, 32, 32, 34, 112, 114, 111, 112, 111,
            115, 101, 114, 95, 102, 101, 101, 95, 114, 101, 99, 105, 112, 105, 101, 110, 116, 34,
            58, 32, 34, 48, 120, 97, 98, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51,
            54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50,
            99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 108, 105, 109, 105,
            116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 117, 115,
            101, 100, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 118, 97, 108, 117, 101,
            34, 58, 32, 34, 49, 34, 10, 32, 32, 125, 44, 10, 32, 32, 34, 101, 120, 101, 99, 117,
            116, 105, 111, 110, 95, 112, 97, 121, 108, 111, 97, 100, 34, 58, 32, 123, 10, 32, 32,
            32, 32, 34, 112, 97, 114, 101, 110, 116, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48,
            120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48,
            49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51,
            100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50,
            48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 102, 101, 101, 95, 114, 101, 99, 105, 112,
            105, 101, 110, 116, 34, 58, 32, 34, 48, 120, 97, 98, 99, 102, 56, 101, 48, 100, 52,
            101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55,
            51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32, 34, 115, 116, 97,
            116, 101, 95, 114, 111, 111, 116, 34, 58, 32, 34, 48, 120, 99, 102, 56, 101, 48, 100,
            52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52,
            55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56, 56, 52, 53,
            54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44, 10, 32, 32,
            32, 32, 34, 114, 101, 99, 101, 105, 112, 116, 115, 95, 114, 111, 111, 116, 34, 58, 32,
            34, 48, 120, 99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50,
            51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57,
            52, 51, 100, 53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100,
            57, 50, 48, 102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 108, 111, 103, 115, 95, 98, 108,
            111, 111, 109, 34, 58, 32, 34, 48, 120, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 48, 34, 44, 10, 32, 32, 32,
            32, 34, 112, 114, 101, 118, 95, 114, 97, 110, 100, 97, 111, 34, 58, 32, 34, 48, 120,
            99, 102, 56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49,
            100, 48, 55, 57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100,
            53, 97, 49, 56, 56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48,
            102, 50, 34, 44, 10, 32, 32, 32, 32, 34, 98, 108, 111, 99, 107, 95, 110, 117, 109, 98,
            101, 114, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115, 95, 108,
            105, 109, 105, 116, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 103, 97, 115,
            95, 117, 115, 101, 100, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 116, 105,
            109, 101, 115, 116, 97, 109, 112, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34,
            101, 120, 116, 114, 97, 95, 100, 97, 116, 97, 34, 58, 32, 34, 48, 120, 99, 102, 56,
            101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57,
            48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56,
            56, 52, 53, 54, 48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44,
            10, 32, 32, 32, 32, 34, 98, 97, 115, 101, 95, 102, 101, 101, 95, 112, 101, 114, 95,
            103, 97, 115, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 34, 98, 108, 111, 99,
            107, 95, 104, 97, 115, 104, 34, 58, 32, 34, 48, 120, 99, 102, 56, 101, 48, 100, 52,
            101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55, 57, 48, 51, 52, 55,
            51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 52, 51, 100, 53, 97, 49, 56, 56, 52, 53, 54,
            48, 51, 54, 55, 101, 56, 50, 48, 56, 100, 57, 50, 48, 102, 50, 34, 44, 10, 32, 32, 32,
            32, 34, 116, 114, 97, 110, 115, 97, 99, 116, 105, 111, 110, 115, 34, 58, 32, 91, 10,
            32, 32, 32, 32, 32, 32, 34, 48, 120, 48, 50, 102, 56, 55, 56, 56, 51, 49, 52, 54, 57,
            54, 54, 56, 51, 48, 51, 102, 53, 49, 100, 56, 52, 51, 98, 57, 97, 99, 57, 102, 57, 56,
            52, 51, 98, 57, 97, 99, 97, 48, 48, 56, 50, 53, 50, 48, 56, 57, 52, 99, 57, 51, 50, 54,
            57, 98, 55, 51, 48, 57, 54, 57, 57, 56, 100, 98, 54, 54, 98, 101, 48, 52, 52, 49, 101,
            56, 51, 54, 100, 56, 55, 51, 53, 51, 53, 99, 98, 57, 99, 56, 56, 57, 52, 97, 49, 57,
            48, 52, 49, 56, 56, 54, 102, 48, 48, 48, 48, 56, 48, 99, 48, 48, 49, 97, 48, 51, 49,
            99, 99, 50, 57, 50, 51, 52, 48, 51, 54, 97, 102, 98, 102, 57, 97, 49, 102, 98, 57, 52,
            55, 54, 98, 52, 54, 51, 51, 54, 55, 99, 98, 49, 102, 57, 53, 55, 97, 99, 48, 98, 57,
            49, 57, 98, 54, 57, 98, 98, 99, 55, 57, 56, 52, 51, 54, 101, 54, 48, 52, 97, 97, 97,
            48, 49, 56, 99, 52, 101, 57, 99, 51, 57, 49, 52, 101, 98, 50, 55, 97, 97, 100, 100, 48,
            98, 57, 49, 101, 49, 48, 98, 49, 56, 54, 53, 53, 55, 51, 57, 102, 99, 102, 56, 99, 49,
            102, 99, 51, 57, 56, 55, 54, 51, 97, 57, 102, 49, 98, 101, 101, 99, 98, 56, 100, 100,
            99, 56, 54, 34, 10, 32, 32, 32, 32, 93, 44, 10, 32, 32, 32, 32, 34, 119, 105, 116, 104,
            100, 114, 97, 119, 97, 108, 115, 34, 58, 32, 91, 10, 32, 32, 32, 32, 32, 32, 123, 10,
            32, 32, 32, 32, 32, 32, 32, 32, 34, 105, 110, 100, 101, 120, 34, 58, 32, 34, 49, 34,
            44, 10, 32, 32, 32, 32, 32, 32, 32, 32, 34, 118, 97, 108, 105, 100, 97, 116, 111, 114,
            95, 105, 110, 100, 101, 120, 34, 58, 32, 34, 49, 34, 44, 10, 32, 32, 32, 32, 32, 32,
            32, 32, 34, 97, 100, 100, 114, 101, 115, 115, 34, 58, 32, 34, 48, 120, 97, 98, 99, 102,
            56, 101, 48, 100, 52, 101, 57, 53, 56, 55, 51, 54, 57, 98, 50, 51, 48, 49, 100, 48, 55,
            57, 48, 51, 52, 55, 51, 50, 48, 51, 48, 50, 99, 99, 48, 57, 34, 44, 10, 32, 32, 32, 32,
            32, 32, 32, 32, 34, 97, 109, 111, 117, 110, 116, 34, 58, 32, 34, 51, 50, 48, 48, 48,
            48, 48, 48, 48, 48, 48, 34, 10, 32, 32, 32, 32, 32, 32, 125, 10, 32, 32, 32, 32, 93,
            10, 32, 32, 125, 44, 10, 32, 32, 34, 115, 105, 103, 110, 97, 116, 117, 114, 101, 34,
            58, 32, 34, 48, 120, 49, 98, 54, 54, 97, 99, 49, 102, 98, 54, 54, 51, 99, 57, 98, 99,
            53, 57, 53, 48, 57, 56, 52, 54, 100, 54, 101, 99, 48, 53, 51, 52, 53, 98, 100, 57, 48,
            56, 101, 100, 97, 55, 51, 101, 54, 55, 48, 97, 102, 56, 56, 56, 100, 97, 52, 49, 97,
            102, 49, 55, 49, 53, 48, 53, 99, 99, 52, 49, 49, 100, 54, 49, 50, 53, 50, 102, 98, 54,
            99, 98, 51, 102, 97, 48, 48, 49, 55, 98, 54, 55, 57, 102, 56, 98, 98, 50, 51, 48, 53,
            98, 50, 54, 97, 50, 56, 53, 102, 97, 50, 55, 51, 55, 102, 49, 55, 53, 54, 54, 56, 100,
            48, 100, 102, 102, 57, 49, 99, 99, 49, 98, 54, 54, 97, 99, 49, 102, 98, 54, 54, 51, 99,
            57, 98, 99, 53, 57, 53, 48, 57, 56, 52, 54, 100, 54, 101, 99, 48, 53, 51, 52, 53, 98,
            100, 57, 48, 56, 101, 100, 97, 55, 51, 101, 54, 55, 48, 97, 102, 56, 56, 56, 100, 97,
            52, 49, 97, 102, 49, 55, 49, 53, 48, 53, 34, 10, 125,
        ];
        match serde_json::from_slice::<SignedBidSubmission>(&json_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_payload_ssz() {
        let ssz_payload: Vec<u8> = vec![];
        match SignedBidSubmission::from_ssz_bytes(&ssz_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_ssz_payload_empty() {
        let ssz_payload = vec![
            178, 184, 84, 0, 0, 0, 0, 0, 189, 50, 145, 133, 77, 200, 34, 183, 236, 88, 89, 37, 205,
            160, 225, 143, 6, 175, 40, 250, 40, 134, 225, 95, 82, 213, 45, 212, 182, 249, 78, 214,
            27, 175, 220, 69, 65, 22, 182, 5, 0, 83, 100, 151, 107, 19, 77, 118, 29, 215, 54, 203,
            71, 136, 210, 92, 131, 87, 131, 180, 109, 174, 177, 33, 182, 122, 81, 72, 160, 50, 41,
            146, 110, 52, 177, 144, 175, 129, 168, 42, 129, 196, 223, 102, 131, 28, 152, 192, 58,
            19, 151, 120, 65, 141, 208, 154, 59, 84, 44, 237, 0, 34, 98, 13, 25, 243, 87, 129, 236,
            230, 220, 54, 133, 89, 114, 126, 230, 92, 41, 82, 121, 51, 33, 152, 2, 156, 147, 149,
            87, 244, 210, 171, 160, 117, 31, 197, 95, 113, 208, 115, 59, 138, 161, 124, 208, 48,
            18, 50, 167, 242, 26, 137, 95, 129, 234, 207, 85, 201, 126, 196, 92, 192, 221, 225, 78,
            114, 86, 52, 12, 200, 32, 65, 90, 96, 34, 167, 209, 201, 58, 53, 128, 195, 201, 1, 0,
            0, 0, 0, 205, 212, 138, 1, 0, 0, 0, 0, 103, 160, 177, 121, 204, 223, 252, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 80, 1, 0, 0, 162, 222,
            245, 66, 55, 191, 235, 29, 146, 105, 54, 94, 133, 59, 84, 105, 246, 139, 127, 74, 213,
            28, 167, 135, 126, 64, 108, 169, 75, 200, 169, 75, 186, 84, 193, 64, 36, 178, 249, 237,
            55, 216, 105, 11, 185, 250, 197, 38, 0, 183, 255, 82, 185, 107, 132, 60, 216, 82, 158,
            158, 204, 36, 151, 160, 236, 213, 219, 131, 114, 226, 4, 145, 86, 224, 250, 147, 52,
            213, 193, 176, 239, 100, 47, 25, 38, 117, 181, 134, 236, 190, 111, 195, 129, 23, 143,
            136, 189, 50, 145, 133, 77, 200, 34, 183, 236, 88, 89, 37, 205, 160, 225, 143, 6, 175,
            40, 250, 40, 134, 225, 95, 82, 213, 45, 212, 182, 249, 78, 214, 182, 74, 48, 57, 159,
            127, 107, 12, 21, 76, 46, 122, 240, 163, 236, 123, 10, 91, 19, 26, 116, 247, 77, 21,
            220, 176, 11, 161, 148, 144, 17, 54, 242, 1, 157, 214, 190, 45, 76, 136, 200, 34, 120,
            109, 249, 5, 97, 165, 80, 25, 56, 153, 180, 16, 250, 52, 161, 49, 38, 38, 219, 60, 65,
            211, 37, 84, 94, 159, 36, 233, 3, 213, 221, 155, 156, 38, 8, 130, 170, 219, 76, 40,
            250, 142, 64, 247, 72, 69, 33, 226, 244, 248, 0, 30, 217, 176, 129, 5, 149, 196, 80,
            56, 8, 68, 175, 97, 9, 68, 33, 129, 4, 210, 28, 34, 0, 40, 170, 106, 21, 10, 134, 49,
            43, 0, 0, 64, 64, 25, 9, 229, 72, 231, 28, 106, 5, 18, 138, 88, 96, 114, 0, 16, 63,
            196, 0, 52, 111, 110, 152, 67, 194, 226, 12, 114, 160, 13, 123, 216, 19, 138, 36, 154,
            131, 32, 134, 151, 192, 208, 61, 118, 51, 193, 130, 72, 165, 143, 232, 130, 132, 36,
            10, 14, 67, 231, 130, 198, 176, 193, 22, 122, 0, 172, 152, 8, 190, 73, 153, 80, 232,
            80, 2, 40, 119, 105, 128, 38, 100, 144, 242, 28, 144, 64, 179, 10, 48, 180, 146, 44,
            58, 122, 248, 80, 186, 8, 242, 0, 1, 70, 234, 133, 4, 1, 233, 120, 3, 142, 18, 5, 9,
            66, 21, 2, 80, 22, 174, 136, 17, 22, 105, 97, 71, 125, 104, 82, 44, 130, 108, 154, 13,
            32, 34, 140, 130, 45, 226, 172, 131, 5, 3, 177, 57, 54, 181, 224, 27, 159, 149, 50,
            236, 35, 34, 199, 12, 73, 28, 26, 33, 97, 133, 51, 194, 132, 41, 155, 24, 146, 7, 207,
            14, 55, 242, 199, 161, 147, 12, 102, 103, 129, 95, 210, 56, 41, 9, 38, 38, 92, 194,
            128, 149, 160, 160, 36, 2, 52, 175, 56, 16, 146, 138, 150, 42, 208, 38, 74, 73, 5, 1,
            138, 2, 161, 153, 98, 129, 110, 157, 10, 57, 253, 76, 128, 147, 83, 56, 167, 65, 220,
            145, 109, 21, 69, 105, 78, 65, 235, 90, 80, 94, 26, 48, 152, 249, 228, 220, 89, 136, 0,
            0, 0, 0, 0, 128, 195, 201, 1, 0, 0, 0, 0, 205, 212, 138, 1, 0, 0, 0, 0, 184, 156, 82,
            100, 0, 0, 0, 0, 0, 2, 0, 0, 255, 18, 249, 112, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 27, 175, 220, 69, 65, 22, 182, 5, 0, 83,
            100, 151, 107, 19, 77, 118, 29, 215, 54, 203, 71, 136, 210, 92, 131, 87, 131, 180, 109,
            174, 177, 33, 31, 2, 0, 0, 31, 2, 0, 0, 73, 108, 108, 117, 109, 105, 110, 97, 116, 101,
            32, 68, 109, 111, 99, 114, 97, 116, 105, 122, 101, 32, 68, 115, 116, 114, 105, 98, 117,
            116, 101, 75, 38, 68, 0, 0, 0, 0, 0, 84, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 86, 104, 29, 0, 0,
            0, 0, 0, 76, 38, 68, 0, 0, 0, 0, 0, 85, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 161, 141, 29, 0, 0,
            0, 0, 0, 77, 38, 68, 0, 0, 0, 0, 0, 86, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 212, 184, 28, 0, 0,
            0, 0, 0, 78, 38, 68, 0, 0, 0, 0, 0, 87, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 157, 132, 29, 0, 0,
            0, 0, 0, 79, 38, 68, 0, 0, 0, 0, 0, 88, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 52, 170, 29, 0, 0,
            0, 0, 0, 80, 38, 68, 0, 0, 0, 0, 0, 89, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 121, 155, 29, 0, 0,
            0, 0, 0, 81, 38, 68, 0, 0, 0, 0, 0, 90, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 225, 68, 29, 0, 0,
            0, 0, 0, 82, 38, 68, 0, 0, 0, 0, 0, 91, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136,
            218, 1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 39, 49, 29, 0, 0, 0,
            0, 0, 83, 38, 68, 0, 0, 0, 0, 0, 92, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218,
            1, 5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 209, 107, 28, 0, 0, 0, 0,
            0, 84, 38, 68, 0, 0, 0, 0, 0, 93, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1,
            5, 124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 110, 85, 29, 0, 0, 0, 0, 0,
            85, 38, 68, 0, 0, 0, 0, 0, 94, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5,
            124, 8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 14, 157, 29, 0, 0, 0, 0, 0, 86,
            38, 68, 0, 0, 0, 0, 0, 95, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124,
            8, 228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 41, 72, 29, 0, 0, 0, 0, 0, 87, 38,
            68, 0, 0, 0, 0, 0, 96, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8,
            228, 193, 186, 168, 250, 166, 41, 129, 156, 42, 105, 5, 29, 0, 0, 0, 0, 0, 88, 38, 68,
            0, 0, 0, 0, 0, 97, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 90, 141, 28, 0, 0, 0, 0, 0, 89, 38, 68, 0,
            0, 0, 0, 0, 98, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 170, 78, 28, 0, 0, 0, 0, 0, 90, 38, 68, 0,
            0, 0, 0, 0, 99, 98, 6, 0, 0, 0, 0, 0, 89, 176, 215, 22, 136, 218, 1, 5, 124, 8, 228,
            193, 186, 168, 250, 166, 41, 129, 156, 42, 209, 28, 29, 0, 0, 0, 0, 0,
        ];

        match SignedBidSubmission::from_ssz_bytes(&ssz_payload) {
            Ok(res) => {
                println!("THIS IS THE RESULT: {:?}", res);
            }
            Err(err) => {
                println!("THIS IS THE ERR: {:?}", err);
            }
        }
    }

    #[tokio::test]
    async fn test_decode_payload_too_large() {
        let payload = vec![0u8; MAX_PAYLOAD_LENGTH + 1];
        let req = build_test_request(payload, false, false).await;
        let mut trace = create_test_submission_trace().await;

        let result = decode_payload(req, &mut trace, false).await;
        match result {
            Ok(_) => panic!("Should have failed"),
            Err(err) => match err {
                BuilderApiError::AxumError(err) => {
                    assert_eq!(err.to_string(), "length limit exceeded");
                }
                _ => panic!("Should have failed with AxumError"),
            },
        }
    }
}
