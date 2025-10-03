use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    B256, U256,
};
use helix_common::{simulator::BlockSimError, utils::utcnow_ms};
use helix_types::{
    mock_public_key_bytes, BlobWithMetadata, BlobsBundleMut, BlsPublicKeyBytes, BuilderBid,
    ExecutionPayloadHeader, KzgCommitments, MergeableOrder, MergeableOrderWithOrigin,
    MergeableOrders, MergedBlock, PayloadAndBlobs, SignedBidSubmission, ValidatorRegistrationData,
};
use parking_lot::RwLock;
use tokio::{
    sync::mpsc::Receiver,
    task::{JoinError, JoinHandle},
};
use tracing::{debug, error, info, warn};

use crate::{
    auctioneer::{BlockMergeRequest, BlockMergeResponse},
    gossiper::types::BroadcastPayloadParams,
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
        let mut last_base_block_hash = B256::ZERO;
        // This handle is used to perform block merging in the background.
        // See `MergingTaskState` for more details on the possible states of this task.
        let mut handle = self.spawn_base_block_fetch_task();
        loop {
            tokio::select! {
                Some(msg) = pool_rx.recv() => {
                    self.process_pool_message(msg, &mut best_orders);
                },
                res = &mut handle => {
                    handle = self.handle_merging_task_result(res, &mut best_orders, &mut last_base_block_hash).await;
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
        best_orders: &mut BestMergeableOrders,
        last_base_block_hash: &mut B256,
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
            MergingTaskState::FetchedBaseBlock {
                bid_slot,
                parent_beacon_block_root,
                best_bid,
                registration_data,
                payload,
            } => {
                let base_block_hash = best_bid.header.block_hash;
                // If we have no new orders, and the base block is the same, we skip merging.
                if !best_orders.has_new_orders() && base_block_hash == *last_base_block_hash {
                    debug!("skipping merging, no new orders and base block unchanged");
                    return self.spawn_base_block_fetch_task();
                }
                // If we have no mergeable orders, we go back to fetching the base block.
                if let Some((mergeable_orders, _)) = best_orders.load(bid_slot) {
                    self.spawn_merging_task(
                        bid_slot,
                        best_bid,
                        registration_data,
                        payload,
                        parent_beacon_block_root,
                        mergeable_orders,
                    )
                } else {
                    self.spawn_base_block_fetch_task()
                }
            }
            // We received a merged block from the simulator.
            // Store it, and go back to fetching the base block again.
            MergingTaskState::GotMergedBlock {
                bid_slot,
                base_bid_value,
                base_block_time_ms,
                proposer_pubkey,
                original_payload,
                response,
            } => {
                // If we are past the slot for the block, skip storing it
                if let Some((_, blobs)) = best_orders.load(bid_slot) {
                    self.local_cache.save_merged_block(MergedBlock {
                        slot: bid_slot,
                        block_number: response.execution_payload.block_number,
                        block_hash: response.execution_payload.block_hash,
                        original_value: base_bid_value,
                        merged_value: response.proposer_value,
                        original_tx_count: original_payload.execution_payload.transactions.len(),
                        merged_tx_count: response.execution_payload.transactions.len(),
                        original_blob_count: original_payload.blobs_bundle.blobs().len(),
                        merged_blob_count: original_payload.blobs_bundle.blobs().len() +
                            response.appended_blobs.len(),
                        builder_inclusions: response.builder_inclusions.clone(),
                    });
                    let _ = self
                        .store_merged_payload(
                            bid_slot,
                            base_block_time_ms,
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
        bid_slot: u64,
        base_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
        parent_beacon_block_root: Option<B256>,
        mergeable_orders: &[MergeableOrderWithOrigin],
    ) -> JoinHandle<MergingTaskResult> {
        let base_block_time_ms = utcnow_ms();
        let proposer_fee_recipient = registration_data.fee_recipient;
        let proposer_pubkey = registration_data.pubkey;

        debug!("merging block");

        let (merge_request, res_rx) = BlockMergeRequest::new(
            base_bid.value,
            proposer_fee_recipient,
            &payload.execution_payload,
            parent_beacon_block_root,
            mergeable_orders,
        );

        let this = self.clone();
        tokio::spawn(async move {
            this.merge_requests_tx.send(merge_request).await?;
            let response = res_rx.await??;

            // Sanity check: if the merged payload has a lower value than the original bid,
            // we return the original bid.
            if base_bid.value >= response.proposer_value {
                return Err(PayloadMergingError::MergedPayloadNotValuable {
                    original: base_bid.value,
                    merged: response.proposer_value,
                });
            }

            Ok(MergingTaskState::new_merged_block(
                bid_slot,
                base_bid.value,
                base_block_time_ms,
                proposer_pubkey,
                payload,
                response,
            ))
        })
    }

    async fn fetch_base_block(&self) -> MergingTaskResult {
        let (head_slot, duty) = self.curr_slot_info.slot_info();
        // If there's no proposer duty next, sleep for a while before continuing.
        let Some(next_duty) = duty else {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Ok(MergingTaskState::RetryFetch);
        };
        let bid_slot = head_slot + 1;

        let Ok(rx) = self.auctioneer_handle.best_mergeable(bid_slot) else {
            warn!("failed sending mergeable request to auctioneer");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::RetryFetch);
        };

        let Ok(maybe_payload) = rx.await else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::RetryFetch);
        };

        // If the current best bid doesn't allow appending, wait a bit before checking again.
        let Some((best_bid, payload)) = maybe_payload else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::RetryFetch);
        };

        let registration_data = next_duty.entry.registration.message;

        let parent_beacon_block_root = self
            .curr_slot_info
            .payload_attributes(payload.execution_payload.parent_hash, bid_slot.into())
            .and_then(|payload_attrs_update| {
                payload_attrs_update.payload_attributes.parent_beacon_block_root
            });

        // Found the base block, we can now start with the merging process.
        Ok(MergingTaskState::new_base_block(
            bid_slot.into(),
            parent_beacon_block_root,
            best_bid,
            registration_data,
            Arc::unwrap_or_clone(payload),
        ))
    }

    fn store_merged_payload(
        &self,
        slot: u64,
        base_block_time_ms: u64,
        proposer_pubkey: BlsPublicKeyBytes,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
        blobs: &HashMap<B256, BlobWithMetadata>,
    ) -> Result<(), PayloadMergingError> {
        let header = ExecutionPayloadHeader::from(&response.execution_payload);

        let mut merged_blobs_bundle = BlobsBundleMut::from_bundle(original_payload.blobs_bundle);
        append_merged_blobs(&mut merged_blobs_bundle, blobs, &response)?;

        let blob_kzg_commitments =
            KzgCommitments::new(merged_blobs_bundle.commitments().iter().copied().collect())
                .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;

        let new_bid = BuilderBid {
            header,
            blob_kzg_commitments,
            execution_requests: Arc::new(response.execution_requests),
            value: response.proposer_value,
            pubkey: mock_public_key_bytes(), // this will be replaced when signing the header
        };

        // Store the payload in the background
        let payload_and_blobs = PayloadAndBlobs {
            execution_payload: response.execution_payload,
            blobs_bundle: merged_blobs_bundle.into_bundle(),
        };

        if self
            .auctioneer_handle
            .gossip_payload(BroadcastPayloadParams {
                execution_payload: payload_and_blobs,
                slot,
                proposer_pub_key: proposer_pubkey,
            })
            .is_err()
        {
            error!("failed sending merged payload to auctioneer");
        };

        // Update best merged block
        let parent_block_hash = new_bid.header.parent_hash;
        self.shared_best_merged.store(slot, base_block_time_ms, parent_block_hash, new_bid);

        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum PayloadMergingError {
    #[error("could not fetch original payload: {_0}")]
    CouldNotFetchOriginalPayload(#[from] ProposerApiError),
    #[error(
        "merged payload value is lower or equal to original bid. original: {original}, merged: {merged}"
    )]
    MergedPayloadNotValuable { original: U256, merged: U256 },
    #[error("blob not found")]
    BlobNotFound,
    #[error("simulator error: {_0}")]
    SimulatorError(#[from] BlockSimError),
    #[error("reached maximum blob count for block")]
    MaxBlobCountReached,
    #[error("failed sending merge request to manager")]
    SendFailed(#[from] tokio::sync::mpsc::error::SendError<BlockMergeRequest>),
    #[error("failed receive response from manager. Was the request dropped?")]
    RecvFailed(#[from] tokio::sync::oneshot::error::RecvError),
}

/// Appends the merged blobs to the original blobs bundle.
fn append_merged_blobs(
    original_blobs_bundle: &mut BlobsBundleMut,
    blobs: &HashMap<B256, BlobWithMetadata>,
    response: &BlockMergeResponse,
) -> Result<(), PayloadMergingError> {
    for vh in &response.appended_blobs {
        let blob_data = blobs.get(vh).ok_or(PayloadMergingError::BlobNotFound)?;
        match blob_data {
            BlobWithMetadata::V1(data) => {
                original_blobs_bundle
                    .push_blob(data.commitment, &vec![data.proof], data.blob.clone())
                    .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;
            }
            BlobWithMetadata::V2(data) => {
                original_blobs_bundle
                    .push_blob(data.commitment, &data.proofs, data.blob.clone())
                    .map_err(|_| PayloadMergingError::MaxBlobCountReached)?;
            }
        }
    }

    Ok(())
}

/// Represents the possible states of the block merging task
#[derive(Debug)]
enum MergingTaskState {
    /// Base block fetching needs to be retried, not necessarily due to an error.
    RetryFetch,
    /// Base block has been fetched successfully.
    /// We can send a request to the simulator to start appending orders.
    FetchedBaseBlock {
        bid_slot: u64,
        parent_beacon_block_root: Option<B256>,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
    },
    /// A merged block has been received from the simulator.
    /// We can now start the process again after updating the best merged block.
    GotMergedBlock {
        bid_slot: u64,
        base_bid_value: U256,
        base_block_time_ms: u64,
        proposer_pubkey: BlsPublicKeyBytes,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
    },
}

impl MergingTaskState {
    fn new_base_block(
        bid_slot: u64,
        parent_beacon_block_root: Option<B256>,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
    ) -> MergingTaskState {
        MergingTaskState::FetchedBaseBlock {
            bid_slot,
            parent_beacon_block_root,
            best_bid,
            registration_data,
            payload,
        }
    }

    fn new_merged_block(
        bid_slot: u64,
        base_bid_value: U256,
        base_block_time_ms: u64,
        proposer_pubkey: BlsPublicKeyBytes,
        original_payload: PayloadAndBlobs,
        response: BlockMergeResponse,
    ) -> MergingTaskState {
        MergingTaskState::GotMergedBlock {
            bid_slot,
            base_bid_value,
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
    has_new_orders: bool,
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
            has_new_orders: false,
        }
    }

    pub fn load(
        &mut self,
        slot: u64,
    ) -> Option<(&[MergeableOrderWithOrigin], &HashMap<B256, BlobWithMetadata>)> {
        // If the request is for another slot, return nothing
        if self.current_slot != slot {
            return None;
        }
        // Reset the new orders flag
        self.has_new_orders = false;
        Some((&self.best_orders, &self.mergeable_blob_bundles))
    }

    pub fn has_new_orders(&self) -> bool {
        self.has_new_orders && !self.best_orders.is_empty()
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
                    // If there were any new orders, we mark it
                    self.has_new_orders = true;

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
