// #[cfg(test)]
// pub mod tests;

mod block_merging;
mod error;
mod get_header;
mod get_payload;
mod gossip;
mod register;
mod types;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use alloy_primitives::{Address, B256};
use axum::{response::IntoResponse, Extension};
use helix_beacon::{multi_beacon_client::MultiBeaconClient, BlockBroadcaster};
use helix_common::{
    bid_sorter::BestGetHeader,
    chain_info::ChainInfo,
    merging_pool::{BestMergeableOrders, MergingPool, MergingPoolMessage},
    signing::RelaySigningContext,
    simulator::BlockSimError,
    utils::utcnow_ms,
    RelayConfig, ValidatorPreferences,
};
use helix_datastore::Auctioneer;
use helix_housekeeper::CurrentSlotInfo;
use helix_types::{
    BlobsBundle, BlsPublicKey, BuilderBid, BuilderBidElectra, ExecutionPayloadHeader,
    MergeableOrderWithOrigin, PayloadAndBlobs, PayloadAndBlobsRef,
};
use hyper::StatusCode;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, warn};
pub use types::*;

use crate::{
    builder::{
        multi_simulator::MultiSimulator, rpc_simulator::BlockMergeResponse, BlockMergeRequest,
    },
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    proposer::block_merging::BestMergedBlock,
    Api,
};

#[derive(Clone)]
pub struct ProposerApi<A: Api> {
    pub auctioneer: Arc<A::Auctioneer>,
    pub db: Arc<A::DatabaseService>,
    pub simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub broadcasters: Vec<Arc<BlockBroadcaster>>,
    pub multi_beacon_client: Arc<MultiBeaconClient>,
    pub metadata_provider: Arc<A::MetadataProvider>,
    pub signing_context: Arc<RelaySigningContext>,

    /// Information about the current head slot and next proposer duty
    pub curr_slot_info: CurrentSlotInfo,
    pub chain_info: Arc<ChainInfo>,
    pub validator_preferences: Arc<ValidatorPreferences>,
    pub relay_config: RelayConfig,
    /// Channel on which to send v3 payload fetch requests.
    pub v3_payload_request: Sender<(u64, B256, BlsPublicKey, Vec<u8>)>,

    /// Set in the sorter loop
    pub shared_best_header: BestGetHeader,

    /// Set in the block merging process
    pub shared_best_merged: BestMergedBlock,
}

impl<A: Api> ProposerApi<A> {
    pub fn new(
        auctioneer: Arc<A::Auctioneer>,
        db: Arc<A::DatabaseService>,
        simulator: MultiSimulator<A::Auctioneer, A::DatabaseService>,
        gossiper: Arc<GrpcGossiperClientManager>,
        metadata_provider: Arc<A::MetadataProvider>,
        signing_context: Arc<RelaySigningContext>,

        broadcasters: Vec<Arc<BlockBroadcaster>>,
        multi_beacon_client: Arc<MultiBeaconClient>,
        chain_info: Arc<ChainInfo>,
        validator_preferences: Arc<ValidatorPreferences>,
        relay_config: RelayConfig,
        v3_payload_request: Sender<(u64, B256, BlsPublicKey, Vec<u8>)>,
        curr_slot_info: CurrentSlotInfo,
        shared_best_header: BestGetHeader,
    ) -> Self {
        Self {
            auctioneer,
            db,
            gossiper,
            broadcasters,
            simulator,
            signing_context,
            multi_beacon_client,
            chain_info,
            metadata_provider,
            validator_preferences,
            relay_config,
            v3_payload_request,
            curr_slot_info,
            shared_best_header,
            shared_best_merged: BestMergedBlock::new(),
        }
    }

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
            let Some(merged_block_bid) = self
                .merge_block(slot, &proposer_pubkey, best_bid, proposer_fee_recipient, &best_orders)
                .await
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
    ) -> Option<BuilderBid> {
        debug!("merging block");

        let block_hash = bid.header().block_hash().0;

        let (mergeable_orders, blobs) = best_orders.load(slot);

        // Get execution payload from auctioneer
        let payload =
            self.get_execution_payload(slot, proposer_pubkey, &block_hash, true).await.ok()?;

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
            .await
            .ok()?;

        Some(new_bid)
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
            warn!(
                original_value = %bid.value(),
                %response.proposer_value,
                "merged payload has lower value than original bid"
            );
            return Err(PayloadMergingError::MergedPayloadHasLowerValue);
        }
        let header = ExecutionPayloadHeader::from(response.execution_payload.to_ref())
            .as_electra()
            .map_err(|_| PayloadMergingError::PayloadNotElectra)?
            .clone();

        let merged_blobs_bundle = append_merged_blobs(payload.blobs_bundle, blobs, &response)?;

        let blob_kzg_commitments = merged_blobs_bundle.commitments.clone();
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
    #[error("merged payload has lower value than original bid")]
    MergedPayloadHasLowerValue,
    #[error("blob not found")]
    BlobNotFound,
    #[error("maximum number of blobs reached")]
    MaxBlobsReached,
    #[error("simulator error: {0}")]
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
        extend_bundle(&mut merged_blobs_bundle, blobs_bundle)?;
        Ok::<(), PayloadMergingError>(())
    })?;
    Ok(merged_blobs_bundle)
}

fn extend_bundle(
    bundle: &mut BlobsBundle,
    other_bundle: BlobsBundle,
) -> Result<(), PayloadMergingError> {
    other_bundle
        .commitments
        .into_iter()
        .try_for_each(|c| bundle.commitments.push(c))
        .map_err(|_| PayloadMergingError::MaxBlobsReached)?;
    other_bundle
        .proofs
        .into_iter()
        .try_for_each(|c| bundle.proofs.push(c))
        .map_err(|_| PayloadMergingError::MaxBlobsReached)?;
    other_bundle
        .blobs
        .into_iter()
        .try_for_each(|c| bundle.blobs.push(c))
        .map_err(|_| PayloadMergingError::MaxBlobsReached)?;

    Ok(())
}

/// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/status>
pub async fn status(Extension(terminating): Extension<Arc<AtomicBool>>) -> impl IntoResponse {
    if terminating.load(Ordering::Relaxed) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}
