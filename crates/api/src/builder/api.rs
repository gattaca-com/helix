use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
};

use alloy_consensus::{Bytes48, TxEip4844, TxType};
use alloy_primitives::{Address, B256};
use axum::{http::StatusCode, response::IntoResponse, Extension};
use bytes::Bytes;
use helix_common::{chain_info::ChainInfo, local_cache::LocalCache, RelayConfig};
use helix_housekeeper::CurrentSlotInfo;
use helix_types::{
    BlobWithMetadata, BlobsBundle, BlockMergingData, BundleOrder, KzgCommitment, MergeableBundle,
    MergeableOrder, MergeableOrders, MergeableTransaction, Order, SignedBidSubmission,
    Transactions,
};
use tracing::error;

use crate::{
    builder::simulator_2::{worker::WorkerJob, Event},
    gossiper::grpc_gossiper::GrpcGossiperClientManager,
    Api,
};

pub(crate) const MAX_PAYLOAD_LENGTH: usize = 1024 * 1024 * 20; // 20MB

#[derive(Clone)]
pub struct BuilderApi<A: Api> {
    pub auctioneer: Arc<LocalCache>,
    pub db: Arc<A::DatabaseService>,
    pub chain_info: Arc<ChainInfo>,
    pub gossiper: Arc<GrpcGossiperClientManager>,
    pub curr_slot_info: CurrentSlotInfo,
    pub relay_config: Arc<RelayConfig>,
    /// Subscriber for TopBid updates, SSZ encoded
    pub top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    // /// Cache of tx roots for v3 submissions
    // pub tx_root_cache: DashMap<B256, (u64, B256)>,
    /// Failsafe: if we fail to demote we pause all optimistic submissions
    pub failsafe_triggered: Arc<AtomicBool>,
    /// Failsafe: if we fail don't have any synced client we pause all optimistic submissions
    pub accept_optimistic: Arc<AtomicBool>,
    pub auctioneer_tx: crossbeam_channel::Sender<Event>,
    pub worker_tx: crossbeam_channel::Sender<WorkerJob>,
    pub metadata_provider: Arc<A::MetadataProvider>,
}

impl<A: Api> BuilderApi<A> {
    pub fn new(
        auctioneer: Arc<LocalCache>,
        db: Arc<A::DatabaseService>,
        chain_info: Arc<ChainInfo>,
        gossiper: Arc<GrpcGossiperClientManager>,
        relay_config: RelayConfig,
        curr_slot_info: CurrentSlotInfo,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        accept_optimistic: Arc<AtomicBool>,
        worker_tx: crossbeam_channel::Sender<WorkerJob>,
        auctioneer_tx: crossbeam_channel::Sender<Event>,
        metadata_provider: Arc<A::MetadataProvider>,
    ) -> Self {
        Self {
            auctioneer,
            db,
            chain_info,
            gossiper,
            relay_config: Arc::new(relay_config),
            curr_slot_info,
            top_bid_tx,
            failsafe_triggered: Arc::new(false.into()),
            accept_optimistic,
            worker_tx,
            auctioneer_tx,
            metadata_provider,
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

    // /// Checks if the builder has enough collateral to submit an optimistic bid.
    // /// Or if the builder is not optimistic.
    // ///
    // /// This function compares the builder's collateral with the block value for a bid
    // submission. /// If the builder's collateral is less than the required value, it returns
    // an error. pub(crate) fn check_builder_collateral(
    //     payload: &impl BidSubmission,
    //     builder_info: &BuilderInfo,
    // ) -> Result<(), BuilderApiError> {
    //     if !builder_info.is_optimistic {
    //         warn!(
    //             builder=%payload.builder_public_key(),
    //             "builder is not optimistic"
    //         );
    //         return Err(BuilderApiError::BuilderNotOptimistic {
    //             builder_pub_key: *payload.builder_public_key(),
    //         });
    //     } else if builder_info.collateral < payload.value() {
    //         warn!(
    //             builder=?payload.builder_public_key(),
    //             collateral=%builder_info.collateral,
    //             collateral_required=%payload.value(),
    //             "builder does not have enough collateral"
    //         );
    //         return Err(BuilderApiError::NotEnoughOptimisticCollateral {
    //             builder_pub_key: *payload.builder_public_key(),
    //             collateral: builder_info.collateral,
    //             collateral_required: payload.value(),
    //             is_optimistic: builder_info.is_optimistic,
    //         });
    //     }

    //     // Builder has enough collateral
    //     Ok(())
    // }
}

#[derive(thiserror::Error, Debug)]
pub enum OrderValidationError {
    #[error("payload fee recipient ({got}) is not builder address ({expected})")]
    FeeRecipientMismatch { got: Address, expected: Address },
    #[error("invalid block merging tx index, got {got} with a tx count of {len}")]
    InvalidTxIndex { got: usize, len: usize },
    #[error("blob transaction does not reference any blobs")]
    EmptyBlobTransaction,
    #[error("blob transaction references blobs not in the block")]
    MissingBlobs,
    #[error("flagged indices reference tx outside of bundle")]
    FlaggedIndicesOutOfBounds,
}

/// Expands the references in [`BlockMergingData`] from the transactions in the
/// payload of the given submission. If any bundle references a transaction not in
/// the payload, it will be silently ignored.
pub fn get_mergeable_orders(
    payload: &SignedBidSubmission,
    merging_data: BlockMergingData,
) -> Result<MergeableOrders, OrderValidationError> {
    let execution_payload = payload.execution_payload_ref();
    if execution_payload.fee_recipient != merging_data.builder_address {
        return Err(OrderValidationError::FeeRecipientMismatch {
            got: merging_data.builder_address,
            expected: execution_payload.fee_recipient,
        });
    }
    let block_blobs_bundles = payload.blobs_bundle();
    let blob_versioned_hashes: Vec<_> =
        block_blobs_bundles.commitments.iter().map(|c| calculate_versioned_hash(*c)).collect();
    let txs = &execution_payload.transactions;

    // Expand all orders to include the tx's bytes, checking for missing blobs.
    let mergeable_orders = merging_data
        .merge_orders
        .into_iter()
        .map(|order| order_to_mergeable(order, txs, &blob_versioned_hashes))
        .collect::<Result<Vec<_>, _>>()?;

    // Stores all block blobs inside a map keyed by versioned hash
    let blobs = blobs_bundle_to_hashmap(blob_versioned_hashes, &block_blobs_bundles);

    Ok(MergeableOrders::new(merging_data.builder_address, mergeable_orders, blobs))
}

fn blobs_bundle_to_hashmap(
    blob_versioned_hashes: Vec<B256>,
    bundle: &BlobsBundle,
) -> HashMap<B256, BlobWithMetadata> {
    blob_versioned_hashes
        .into_iter()
        .zip(bundle.commitments.iter())
        .zip(bundle.proofs.iter())
        .zip(bundle.blobs.iter())
        .map(|(((versioned_hash, commitment), proof), blob)| {
            let commitment = *commitment;
            let proof = *proof;
            let blob = blob.clone();
            (versioned_hash, BlobWithMetadata { commitment, proof, blob })
        })
        .collect()
}

fn order_to_mergeable(
    order: Order,
    txs: &Transactions,
    blob_versioned_hashes: &[B256],
) -> Result<MergeableOrder, OrderValidationError> {
    match order {
        Order::Tx(tx) => {
            let Some(raw_tx) = txs.get(tx.index) else {
                return Err(OrderValidationError::InvalidTxIndex { got: tx.index, len: txs.len() });
            };
            if is_blob_transaction(raw_tx) {
                // If the tx references bundles not in the block, we drop it
                validate_blobs(raw_tx, blob_versioned_hashes)?;
            }

            let transaction = Bytes::from(raw_tx.to_vec());
            let mergeable_tx =
                MergeableTransaction { transaction, can_revert: tx.can_revert }.into();
            Ok(mergeable_tx)
        }
        Order::Bundle(bundle) => {
            bundle.validate().map_err(|_| OrderValidationError::FlaggedIndicesOutOfBounds)?;

            let transactions = bundle
                .txs
                .iter()
                .map(|tx_index| {
                    let Some(raw_tx) = txs.get(*tx_index) else {
                        return Err(OrderValidationError::InvalidTxIndex {
                            got: *tx_index,
                            len: txs.len(),
                        });
                    };

                    if is_blob_transaction(raw_tx) {
                        // If the tx references bundles not in the block, we drop the bundle
                        validate_blobs(raw_tx, blob_versioned_hashes)?;
                    }

                    Ok(Bytes::from_owner(raw_tx.to_vec()))
                })
                .collect::<Result<_, OrderValidationError>>()?;

            let BundleOrder { reverting_txs, dropping_txs, .. } = bundle;

            let mergeable_bundle =
                MergeableBundle { transactions, reverting_txs, dropping_txs }.into();
            Ok(mergeable_bundle)
        }
    }
}

fn is_blob_transaction(raw_tx: &[u8]) -> bool {
    // First byte is always the transaction type, or >= 0xc0 for legacy
    // (source: https://eips.ethereum.org/EIPS/eip-2718)
    raw_tx.first().is_some_and(|&b| b == TxType::Eip4844)
}

fn get_tx_versioned_hashes(mut raw_tx: &[u8]) -> Vec<B256> {
    use alloy_consensus::transaction::RlpEcdsaDecodableTx;
    TxEip4844::rlp_decode_with_signature(&mut raw_tx)
        .map(|(b, _)| b.blob_versioned_hashes)
        .unwrap_or(vec![])
}

fn validate_blobs(
    raw_tx: &[u8],
    blob_versioned_hashes: &[B256],
) -> Result<(), OrderValidationError> {
    let versioned_hashes = get_tx_versioned_hashes(raw_tx);
    let num_blobs = versioned_hashes.len();
    if num_blobs == 0 {
        return Err(OrderValidationError::EmptyBlobTransaction);
    }
    let mut missing_blobs =
        versioned_hashes.iter().map(|h| !blob_versioned_hashes.iter().any(|vh| vh == h));
    if missing_blobs.any(|f| f) {
        return Err(OrderValidationError::MissingBlobs);
    }
    Ok(())
}

fn calculate_versioned_hash(commitment: Bytes48) -> B256 {
    KzgCommitment(*commitment).calculate_versioned_hash()
}
