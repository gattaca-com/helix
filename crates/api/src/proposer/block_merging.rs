use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use helix_common::{
    merging_pool::{BestMergeableOrders, MergingPool, MergingPoolMessage},
    simulator::BlockSimError,
    utils::utcnow_ms,
};
use helix_datastore::Auctioneer;
use helix_types::{
    BlobsBundle, BlsPublicKey, BuilderBid, BuilderBidElectra, ExecutionPayloadHeader,
    KzgCommitment, KzgCommitments, MergeableOrderWithOrigin, PayloadAndBlobs, PayloadAndBlobsRef,
};
use parking_lot::RwLock;
use tracing::{debug, error, warn};

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
        pool_rx: tokio::sync::mpsc::Receiver<MergingPoolMessage>,
    ) {
        let shared_best_orders = BestMergeableOrders::new();
        let merging_pool = MergingPool::new(pool_rx, shared_best_orders.clone());
        tokio::select! {
            _ = self.block_merging_loop(shared_best_orders) => {},
            _ = merging_pool.run() => {},
        }
    }

    pub async fn block_merging_loop(&self, best_orders: BestMergeableOrders) {
        loop {
            let (_head_slot, duty) = self.curr_slot_info.slot_info();
            let Some(next_duty) = duty else {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            };
            let slot = next_duty.slot.into();
            let best_header_opt = self.shared_best_header.load_any(slot);
            let Some((best_bid, merging_preferences)) = best_header_opt else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };

            if !merging_preferences.allow_appending {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            };
            let base_block_fetched = utcnow_ms();
            let proposer_fee_recipient = next_duty.entry.registration.message.fee_recipient;
            let proposer_pubkey = next_duty.entry.registration.message.pubkey;

            // Merge block
            let Ok(merged_block_bid) = self
                .merge_block(slot, &proposer_pubkey, best_bid, proposer_fee_recipient, &best_orders)
                .await
                .inspect_err(|err| warn!(%err, "failed when merging block"))
            else {
                continue;
            };

            // Update best merged block
            let parent_block_hash = merged_block_bid.header().parent_hash().0;
            self.shared_best_merged.store(
                slot,
                base_block_fetched,
                parent_block_hash,
                merged_block_bid,
            );
        }
    }

    async fn merge_block(
        &self,
        slot: u64,
        proposer_pubkey: &BlsPublicKey,
        bid: BuilderBid,
        proposer_fee_recipient: Address,
        best_orders: &BestMergeableOrders,
    ) -> Result<BuilderBid, PayloadMergingError> {
        debug!("merging block");

        let block_hash = bid.header().block_hash().0;

        let (mergeable_orders, blobs) = best_orders.load(slot);

        // Get execution payload from auctioneer
        let payload = self.get_execution_payload(slot, proposer_pubkey, &block_hash, true).await?;

        let new_bid = self
            .append_transactions_to_payload(
                slot,
                proposer_fee_recipient,
                proposer_pubkey,
                bid,
                payload,
                mergeable_orders,
                blobs,
            )
            .await?;

        Ok(new_bid)
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
