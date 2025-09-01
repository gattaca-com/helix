use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    B256, U256,
};
use helix_common::{bid_submission::BidSubmission, simulator::BlockSimError, utils::utcnow_ms};
use helix_datastore::Auctioneer;
use helix_types::{
    BlobWithMetadata, BlobsBundle, BlsPublicKey, BuilderBid, BuilderBidElectra,
    ExecutionPayloadHeader, ExecutionPayloadRef, KzgCommitment, KzgCommitments, MergeableOrder,
    MergeableOrderWithOrigin, MergeableOrders, PayloadAndBlobs, PublicKeyBytes,
    SignedBidSubmission, ValidatorRegistrationData,
};
use parking_lot::RwLock;
use tokio::{
    sync::mpsc::Receiver,
    task::{JoinError, JoinHandle},
};
use tracing::{debug, error, info, warn};

use crate::{
    builder::{rpc_simulator::BlockMergeResponse, BlockMergeRequest},
    proposer::{error::ProposerApiError, ProposerApi},
    Api,
};

struct BestMergedBlockEntry {
    /// Slot of the merged block
    slot: u64,
    /// Timestamp when the base block was fetched
    base_block_time_ms: u64,
    /// Hash of the parent of the merged block
    parent_block_hash: B256,
    /// The merged block bid
    bid: BuilderBid,
}

#[derive(Clone)]
pub struct BestMergedBlock(Arc<RwLock<Option<BestMergedBlockEntry>>>);

impl Default for BestMergedBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergedBlock {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }

    pub fn store(
        &self,
        slot: u64,
        base_block_time_ms: u64,
        parent_block_hash: B256,
        bid: BuilderBid,
    ) {
        let mut guard = self.0.write();
        *guard = Some(BestMergedBlockEntry { slot, base_block_time_ms, parent_block_hash, bid });
    }

    /// Loads the best merged block for the given slot and parent block hash.
    /// Returns a timestamp for when the base block was fetched, and the bid of the merged block.
    pub fn load(&self, slot: u64, parent_block_hash: &B256) -> Option<(u64, BuilderBid)> {
        self.0
            .read()
            .as_ref()
            .filter(|entry| entry.slot == slot && entry.parent_block_hash == *parent_block_hash)
            .map(|entry| (entry.base_block_time_ms, entry.bid.clone()))
    }
}

impl<A: Api> ProposerApi<A> {
    pub async fn process_block_merging(self: Arc<Self>, mut pool_rx: Receiver<MergingPoolMessage>) {
        let mut best_orders = BestMergeableOrders::new();
        // This handle is used to perform block merging in the background.
        // See `MergingTaskState` for more details on the possible states of this task.
        let mut handle = self.spawn_base_block_fetch_task();
        loop {
            tokio::select! {
                Some(msg) = pool_rx.recv() => {
                    self.process_pool_message(msg, &mut best_orders);
                },
                res = &mut handle => {
                    handle = self.handle_merging_task_result(res, &best_orders).await;
                },
            }
        }
    }

    fn process_pool_message(&self, msg: MergingPoolMessage, best_orders: &mut BestMergeableOrders) {
        let MergingPoolMessage { bid_value, orders } = msg;
        info!(?bid_value, "received new mergeable orders");
        let (head_slot, _) = self.curr_slot_info.slot_info();
        let current_slot = (head_slot + 1).into();
        best_orders.insert_orders(current_slot, bid_value, orders);
    }

    async fn handle_merging_task_result(
        self: &Arc<Self>,
        result: Result<MergingTaskResult, JoinError>,
        best_orders: &BestMergeableOrders,
    ) -> JoinHandle<MergingTaskResult> {
        // Start again on any errors.
        let Ok(result) = result.inspect_err(|err| warn!(%err, "merging task panicked")) else {
            return self.spawn_base_block_fetch_task();
        };
        let Ok(task_result) = result.inspect_err(|err| warn!(%err, "failed when merging payload"))
        else {
            return self.spawn_base_block_fetch_task();
        };
        match task_result {
            // If we couldn't fetch a base block, try again.
            MergingTaskState::RetryFetch => self.spawn_base_block_fetch_task(),
            // We fetched the base block, so we start a merging task.
            MergingTaskState::FetchedBaseBlock { slot, best_bid, registration_data, payload } => {
                // If we have no mergeable orders, we go back to fetching the base block.
                if let Some((mergeable_orders, _)) = best_orders.load(slot) {
                    self.spawn_merging_task(
                        slot,
                        best_bid,
                        registration_data,
                        payload,
                        mergeable_orders,
                    )
                } else {
                    self.spawn_base_block_fetch_task()
                }
            }
            // We received a merged block from the simulator.
            // Store it, and go back to fetching the base block again.
            MergingTaskState::GotMergedBlock {
                slot,
                base_block_time_ms,
                builder_pubkey,
                proposer_pubkey,
                original_payload,
                response,
            } => {
                // If we are past the slot for the block, skip storing it
                if let Some((_, blobs)) = best_orders.load(slot) {
                    let _ = self
                        .store_merged_payload(
                            slot,
                            base_block_time_ms,
                            builder_pubkey,
                            proposer_pubkey,
                            original_payload,
                            response,
                            blobs,
                        )
                        .inspect_err(|err| warn!(%err, "failed to store merged payload"));
                }

                self.spawn_base_block_fetch_task()
            }
        }
    }

    fn spawn_base_block_fetch_task(self: &Arc<Self>) -> JoinHandle<MergingTaskResult> {
        tokio::spawn({
            let this = self.clone();
            async move { this.fetch_base_block().await }
        })
    }

    fn spawn_merging_task(
        self: &Arc<Self>,
        slot: u64,
        base_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
        mergeable_orders: &[MergeableOrderWithOrigin],
    ) -> JoinHandle<MergingTaskResult> {
        let base_block_time_ms = utcnow_ms();
        let proposer_fee_recipient = registration_data.fee_recipient;
        let proposer_pubkey = registration_data.pubkey.clone();

        debug!("merging block");

        let merge_request = BlockMergeRequest::new(
            *base_bid.value(),
            proposer_fee_recipient,
            ExecutionPayloadRef::from(&payload.execution_payload),
            mergeable_orders,
        );

        let this = self.clone();
        tokio::spawn(async move {
            let response = this.simulator.process_merge_request(merge_request).await?;

            // Sanity check: if the merged payload has a lower value than the original bid,
            // we return the original bid.
            if base_bid.value() >= &response.proposer_value {
                return Err(PayloadMergingError::MergedPayloadNotValuable {
                    original: *base_bid.value(),
                    merged: response.proposer_value,
                });
            }

            Ok(MergingTaskState::new_merged_block(
                slot,
                base_block_time_ms,
                *base_bid.pubkey(),
                proposer_pubkey,
                payload,
                response,
            ))
        })
    }

    async fn fetch_base_block(&self) -> MergingTaskResult {
        let (_head_slot, duty) = self.curr_slot_info.slot_info();
        // If there's no proposer duty next, sleep for a while before continuing.
        let Some(next_duty) = duty else {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Ok(MergingTaskState::RetryFetch);
        };
        let slot = next_duty.slot.into();
        let best_header_opt = self.shared_best_header.load_any(slot);
        // If there are no bids, wait for a moment to avoid busy waiting.
        let Some((best_bid, merging_preferences)) = best_header_opt else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::RetryFetch);
        };
        // If the current best bid doesn't allow appending, wait a bit before checking again.
        if !merging_preferences.allow_appending {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::RetryFetch);
        };
        let registration_data = next_duty.entry.registration.message;
        let block_hash = best_bid.header().block_hash().0;
        // Try to fetch the best bid's block.
        let payload = self
            .get_execution_payload(slot, &registration_data.pubkey, &block_hash, true)
            .await
            .inspect_err(|err| warn!(%err, "failed to fetch base block"))?;

        // Found the base block, we can now start with the merging process.
        Ok(MergingTaskState::new_base_block(slot, best_bid, registration_data, payload))
    }

    fn store_merged_payload(
        &self,
        slot: u64,
        base_block_time_ms: u64,
        builder_pubkey: PublicKeyBytes,
        proposer_pubkey: BlsPublicKey,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
        blobs: &HashMap<B256, BlobWithMetadata>,
    ) -> Result<(), PayloadMergingError> {
        let header = ExecutionPayloadHeader::from(response.execution_payload.to_ref())
            .as_electra()
            .map_err(|_| PayloadMergingError::PayloadNotElectra)?
            .clone();

        let merged_blobs_bundle =
            append_merged_blobs(original_payload.blobs_bundle, blobs, &response)?;

        let blob_kzg_commitments = KzgCommitments::new(
            merged_blobs_bundle.commitments.iter().map(|k| KzgCommitment(**k)).collect(),
        )
        .unwrap();
        let block_hash = response.execution_payload.block_hash().0;

        let new_bid = BuilderBidElectra {
            header,
            blob_kzg_commitments,
            execution_requests: response.execution_requests,
            value: response.proposer_value,
            pubkey: builder_pubkey,
        };

        // Store the payload in the background
        let payload_and_blobs = PayloadAndBlobs {
            execution_payload: response.execution_payload,
            blobs_bundle: merged_blobs_bundle,
        };
        // We just log the errors as we can't really do anything about them
        self.auctioneer.save_execution_payload(
            slot,
            &proposer_pubkey,
            &block_hash,
            payload_and_blobs,
        );

        // Update best merged block
        let parent_block_hash = new_bid.header.parent_hash.0;
        self.shared_best_merged.store(slot, base_block_time_ms, parent_block_hash, new_bid.into());

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum PayloadMergingError {
    #[error("could not fetch original payload: {_0}")]
    CouldNotFetchOriginalPayload(#[from] ProposerApiError),
    #[error("merged payload value is lower or equal to original bid. original: {original}, merged: {merged}")]
    MergedPayloadNotValuable { original: U256, merged: U256 },
    #[error("blob not found")]
    BlobNotFound,
    #[error("simulator error: {_0}")]
    SimulatorError(#[from] BlockSimError),
    #[error("payload not from electra fork")]
    PayloadNotElectra,
}

/// Appends the merged blobs to the original blobs bundle.
fn append_merged_blobs(
    original_blobs_bundle: BlobsBundle,
    blobs: &HashMap<B256, BlobWithMetadata>,
    response: &BlockMergeResponse,
) -> Result<BlobsBundle, PayloadMergingError> {
    let mut merged_blobs_bundle = original_blobs_bundle;

    response.appended_blobs.iter().try_for_each(|vh| {
        let blob_bundle = blobs.get(vh).ok_or(PayloadMergingError::BlobNotFound)?;
        extend_bundle(&mut merged_blobs_bundle, blob_bundle.clone());
        Ok::<(), PayloadMergingError>(())
    })?;
    Ok(merged_blobs_bundle)
}

fn extend_bundle(bundle: &mut BlobsBundle, other_bundle: BlobWithMetadata) {
    bundle.commitments.push(other_bundle.commitment);
    bundle.proofs.push(other_bundle.proof);
    bundle.blobs.push(other_bundle.blob);
}

/// Represents the possible states of the block merging task
#[derive(Debug)]
enum MergingTaskState {
    /// Base block fetching needs to be retried, not necessarily due to an error.
    RetryFetch,
    /// Base block has been fetched successfully.
    /// We can send a request to the simulator to start appending orders.
    FetchedBaseBlock {
        slot: u64,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
    },
    /// A merged block has been received from the simulator.
    /// We can now start the process again after updating the best merged block.
    GotMergedBlock {
        slot: u64,
        base_block_time_ms: u64,
        builder_pubkey: PublicKeyBytes,
        proposer_pubkey: BlsPublicKey,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
    },
}

impl MergingTaskState {
    fn new_base_block(
        slot: u64,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
    ) -> MergingTaskState {
        MergingTaskState::FetchedBaseBlock { slot, best_bid, registration_data, payload }
    }

    fn new_merged_block(
        slot: u64,
        base_block_time_ms: u64,
        builder_pubkey: PublicKeyBytes,
        proposer_pubkey: BlsPublicKey,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
    ) -> MergingTaskState {
        MergingTaskState::GotMergedBlock {
            slot,
            builder_pubkey,
            proposer_pubkey,
            base_block_time_ms,
            original_payload,
            response,
        }
    }
}

type MergingTaskResult = Result<MergingTaskState, PayloadMergingError>;

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    index: usize,
}

#[derive(Debug, Clone)]
pub struct BestMergeableOrders {
    current_slot: u64,
    order_map: HashMap<MergeableOrder, OrderMetadata>,
    best_orders: Vec<MergeableOrderWithOrigin>,
    mergeable_blob_bundles: HashMap<B256, BlobWithMetadata>,
}

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    pub fn new() -> Self {
        Self {
            current_slot: 0,
            order_map: HashMap::with_capacity(5000),
            mergeable_blob_bundles: HashMap::with_capacity(5000),
            best_orders: Vec::with_capacity(5000),
        }
    }

    pub fn load(
        &self,
        slot: u64,
    ) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        // If the request is for another slot, return nothing
        if self.current_slot != slot {
            return None;
        }
        Some((&self.best_orders, &self.mergeable_blob_bundles))
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    pub fn insert_orders(&mut self, slot: u64, bid_value: U256, mergeable_orders: MergeableOrders) {
        // If the orders are for a newer slot, reset current state.
        if self.current_slot < slot {
            self.reset(slot);
        }
        let origin = mergeable_orders.origin;
        // Insert each order into the order map
        mergeable_orders.orders.into_iter().for_each(|o| {
            // Collect all blobs for this order
            match self.order_map.entry(o) {
                // If the order already exists, keep the one with the highest bid
                Entry::Occupied(mut e) if e.get().value < bid_value => {
                    // We update the value of the bid to the highest
                    e.get_mut().value = bid_value;
                    // and the origin for the order
                    self.best_orders[e.get().index].origin = origin;
                }
                Entry::Occupied(_) => {}
                // Otherwise, insert the new order
                Entry::Vacant(e) => {
                    let index = self.best_orders.len();
                    // We insert the order to our list of orders
                    self.best_orders.push(MergeableOrderWithOrigin::new(origin, e.key().clone()));
                    // and insert the metadata
                    e.insert(OrderMetadata { value: bid_value, index });
                }
            }
        });
        // Insert new blobs
        self.mergeable_blob_bundles.extend(mergeable_orders.blobs);
    }

    fn reset(&mut self, slot: u64) {
        self.current_slot = slot;
        self.order_map.clear();
        self.best_orders.clear();
        self.mergeable_blob_bundles.clear();
    }
}

pub struct MergingPoolMessage {
    bid_value: U256,
    orders: MergeableOrders,
}

impl MergingPoolMessage {
    pub fn new(submission: &SignedBidSubmission, orders: MergeableOrders) -> Self {
        Self { bid_value: submission.value(), orders }
    }
}
