use std::{collections::hash_map::Entry, sync::Arc};

use alloy_primitives::{
    map::foldhash::{HashMap, HashMapExt},
    Address, B256, U256,
};
use helix_common::{bid_submission::BidSubmission, simulator::BlockSimError, utils::utcnow_ms};
use helix_datastore::Auctioneer;
use helix_types::{
    BlobsBundle, BlsPublicKey, BuilderBid, BuilderBidElectra, ExecutionPayloadHeader,
    KzgCommitment, KzgCommitments, MergeableOrder, MergeableOrderWithOrigin, MergeableOrders,
    PayloadAndBlobs, PayloadAndBlobsRef, SignedBidSubmission, ValidatorRegistrationData,
};
use parking_lot::RwLock;
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
    base_block_time_ns: u64,
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
        base_block_time_ns: u64,
        parent_block_hash: B256,
        bid: BuilderBid,
    ) {
        let mut guard = self.0.write();
        *guard = Some(BestMergedBlockEntry { slot, base_block_time_ns, parent_block_hash, bid });
    }

    /// Loads the best merged block for the given slot and parent block hash.
    /// Returns a timestamp for when the base block was fetched, and the bid of the merged block.
    pub fn load(&self, slot: u64, parent_block_hash: &B256) -> Option<(u64, BuilderBid)> {
        self.0
            .read()
            .as_ref()
            .filter(|entry| entry.slot == slot && entry.parent_block_hash == *parent_block_hash)
            .map(|entry| (entry.base_block_time_ns, entry.bid.clone()))
    }
}

impl<A: Api> ProposerApi<A> {
    pub async fn process_block_merging(
        self: Arc<Self>,
        mut pool_rx: tokio::sync::mpsc::Receiver<MergingPoolMessage>,
    ) {
        let mut best_orders = BestMergeableOrders::new();
        let mut handle = tokio::spawn({
            let this = self.clone();
            async move { this.fetch_base_block().await }
        });
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
        result: Result<MergingTaskResult, tokio::task::JoinError>,
        best_orders: &BestMergeableOrders,
    ) -> tokio::task::JoinHandle<MergingTaskResult> {
        let this = self.clone();
        let Ok(result) = result.inspect_err(|err| warn!(%err, "merging task panicked")) else {
            return tokio::spawn(async move { this.fetch_base_block().await });
        };
        let Ok(task_result) = result.inspect_err(|err| warn!(%err, "failed when merging payload"))
        else {
            return tokio::spawn(async move { this.fetch_base_block().await });
        };
        // Check if the task could fetch a base block, otherwise we try again
        let MergingTaskState::FetchedBaseBlock { slot, best_bid, registration_data, payload } =
            task_result
        else {
            return tokio::spawn(async move { this.fetch_base_block().await });
        };
        let (mergeable_orders, blobs) = best_orders.load(slot);

        tokio::spawn(async move {
            this.merge_block(slot, best_bid, registration_data, payload, mergeable_orders, blobs)
                .await
        })
    }

    async fn fetch_base_block(&self) -> MergingTaskResult {
        let (_head_slot, duty) = self.curr_slot_info.slot_info();
        let Some(next_duty) = duty else {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            return Ok(MergingTaskState::FetchBaseBlock);
        };
        let slot = next_duty.slot.into();
        let best_header_opt = self.shared_best_header.load_any(slot);
        let Some((best_bid, merging_preferences)) = best_header_opt else {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::FetchBaseBlock);
        };

        if !merging_preferences.allow_appending {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(MergingTaskState::FetchBaseBlock);
        };
        let registration_data = next_duty.entry.registration.message;
        let block_hash = best_bid.header().block_hash().0;
        let payload = self
            .get_execution_payload(slot, &registration_data.pubkey, &block_hash, true)
            .await
            .inspect_err(|err| warn!(%err, "failed to fetch base block"))?;
        Ok(MergingTaskState::new_base_block(slot, best_bid, registration_data, payload))
    }

    async fn merge_block(
        &self,
        slot: u64,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
        mergeable_orders: Vec<MergeableOrderWithOrigin>,
        blobs: Vec<(usize, usize, BlobsBundle)>,
    ) -> MergingTaskResult {
        let base_block_fetched = utcnow_ms();
        let proposer_fee_recipient = registration_data.fee_recipient;
        let proposer_pubkey = registration_data.pubkey.clone();

        debug!("merging block");

        // Merge block
        let merged_block_bid = self
            .append_transactions_to_payload(
                slot,
                proposer_fee_recipient,
                &proposer_pubkey,
                best_bid.clone(),
                payload.clone(),
                mergeable_orders,
                blobs,
            )
            .await
            .inspect_err(|err| warn!(%err, "failed when merging block"))?;

        // Update best merged block
        let parent_block_hash = merged_block_bid.header().parent_hash().0;
        self.shared_best_merged.store(
            slot,
            base_block_fetched,
            parent_block_hash,
            merged_block_bid,
        );

        Ok(MergingTaskState::MergedBlock)
    }

    /// Appends transactions to the payload and returns a new bid.
    /// The payload referenced in the bid is stored in the DB.
    /// This function might return the original bid if somehow the merged payload has a lower value.
    async fn append_transactions_to_payload(
        &self,
        slot: u64,
        proposer_fee_recipient: Address,
        proposer_pubkey: &BlsPublicKey,
        bid: BuilderBid,
        payload: PayloadAndBlobs,
        merging_data: Vec<MergeableOrderWithOrigin>,
        blobs: Vec<(usize, usize, BlobsBundle)>,
    ) -> Result<BuilderBid, PayloadMergingError> {
        let merge_request = BlockMergeRequest::new(
            *bid.value(),
            proposer_fee_recipient,
            payload.execution_payload,
            payload.blobs_bundle.clone(),
            merging_data,
        );

        let response = self.simulator.process_merge_request(merge_request).await?;

        // Sanity check: if the merged payload has a lower value than the original bid,
        // we return the original bid.
        if bid.value() >= &response.proposer_value {
            return Err(PayloadMergingError::MergedPayloadNotValuable {
                original: *bid.value(),
                merged: response.proposer_value,
            });
        }
        let header = ExecutionPayloadHeader::from(response.execution_payload.to_ref())
            .as_electra()
            .map_err(|_| PayloadMergingError::PayloadNotElectra)?
            .clone();

        let merged_blobs_bundle = append_merged_blobs(payload.blobs_bundle, blobs, &response)?;

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
            pubkey: *bid.pubkey(),
        };

        // Store the payload in the background
        tokio::task::spawn({
            let auctioneer = self.auctioneer.clone();
            let proposer_pubkey = proposer_pubkey.clone();
            async move {
                let payload_and_blobs = PayloadAndBlobsRef {
                    execution_payload: (&response.execution_payload).into(),
                    blobs_bundle: &merged_blobs_bundle,
                };
                // We just log the errors as we can't really do anything about them
                if let Err(err) = auctioneer
                    .save_execution_payload(slot, &proposer_pubkey, &block_hash, payload_and_blobs)
                    .await
                {
                    error!(%err, "failed to store merged payload in auctioneer");
                }
            }
        });

        Ok(new_bid.into())
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
    mut blobs: Vec<(usize, usize, BlobsBundle)>,
    response: &BlockMergeResponse,
) -> Result<BlobsBundle, PayloadMergingError> {
    let mut merged_blobs_bundle = original_blobs_bundle;

    response.appended_blob_order_indices.iter().try_for_each(|(i, j)| {
        let blob_bundle_index = blobs
            .binary_search_by_key(&(i, j), |(k, l, _bundle)| (k, l))
            .map_err(|_| PayloadMergingError::BlobNotFound)?;

        let (_, _, blobs_bundle) = std::mem::take(&mut blobs[blob_bundle_index]);
        extend_bundle(&mut merged_blobs_bundle, blobs_bundle);
        Ok::<(), PayloadMergingError>(())
    })?;
    Ok(merged_blobs_bundle)
}

fn extend_bundle(bundle: &mut BlobsBundle, other_bundle: BlobsBundle) {
    bundle.commitments.extend(other_bundle.commitments);
    bundle.proofs.extend(other_bundle.proofs);
    bundle.blobs.extend(other_bundle.blobs);
}

#[derive(Debug, Clone)]
enum MergingTaskState {
    FetchBaseBlock,
    FetchedBaseBlock {
        slot: u64,
        best_bid: BuilderBid,
        registration_data: ValidatorRegistrationData,
        payload: PayloadAndBlobs,
    },
    MergedBlock,
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
}

type MergingTaskResult = Result<MergingTaskState, PayloadMergingError>;

#[derive(Debug, Clone)]
struct OrderMetadata {
    value: U256,
    origin: Address,
    blobs: Vec<(usize, BlobsBundle)>,
}

#[derive(Debug, Clone)]
pub struct BestMergeableOrders {
    current_slot: u64,
    // TODO: change structures to avoid copies
    order_map: HashMap<MergeableOrder, OrderMetadata>,
}

impl Default for BestMergeableOrders {
    fn default() -> Self {
        Self::new()
    }
}

impl BestMergeableOrders {
    pub fn new() -> Self {
        Self { current_slot: 0, order_map: HashMap::with_capacity(5000) }
    }

    pub fn load(
        &self,
        slot: u64,
    ) -> (Vec<MergeableOrderWithOrigin>, Vec<(usize, usize, BlobsBundle)>) {
        // If the request is for another slot, return nothing
        if self.current_slot != slot {
            return (Vec::new(), Vec::new());
        }
        let mut blobs = vec![];
        // Clone the orders and return them, collecting blobs in the process
        let orders = self
            .order_map
            .iter()
            .enumerate()
            .map(|(i, (order, metadata))| {
                let order_with_origin =
                    MergeableOrderWithOrigin::new(metadata.origin, order.clone());
                blobs.extend(metadata.blobs.iter().map(|(j, blob)| (i, *j, blob.clone())));
                order_with_origin
            })
            .collect();
        (orders, blobs)
    }

    /// Inserts the orders into the merging pool.
    /// Any duplicates are discarded, unless the bid value is higher than the
    /// existing one, in which case they replace the old order.
    pub fn insert_orders(&mut self, slot: u64, bid_value: U256, mergeable_orders: MergeableOrders) {
        // If the orders are for another slot, discard them
        if self.current_slot < slot {
            self.reset(slot);
        }
        let origin = mergeable_orders.origin;

        let mut blob_iter = mergeable_orders.blobs.into_iter().fuse().peekable();
        // Insert each order into the order map
        mergeable_orders.orders.into_iter().enumerate().for_each(|(i, o)| {
            // Collect all blobs for this order
            let mut blobs = vec![];
            while blob_iter.peek().is_some() && blob_iter.peek().unwrap().0 == i {
                let (_i, j, blob) = blob_iter.next().unwrap();
                blobs.push((j, blob));
            }
            match self.order_map.entry(o) {
                // If the order already exists, keep the one with the highest bid
                Entry::Occupied(mut e) if e.get().value < bid_value => {
                    *e.get_mut() = OrderMetadata { value: bid_value, origin, blobs };
                }
                Entry::Occupied(_) => {}
                // Otherwise, insert the new order
                Entry::Vacant(e) => {
                    e.insert(OrderMetadata { value: bid_value, origin, blobs });
                }
            }
        });
    }

    fn reset(&mut self, slot: u64) {
        self.current_slot = slot;
        self.order_map.clear();
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
