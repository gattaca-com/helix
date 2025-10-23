use std::{collections::HashMap, sync::Arc};

use alloy_consensus::{Bytes48, TxEip4844, TxType};
use alloy_primitives::B256;
use axum::{Extension, http::StatusCode, response::IntoResponse};
use bytes::Bytes;
use helix_common::{RelayConfig, local_cache::LocalCache};
use helix_types::{
    BlobWithMetadata, BlobWithMetadataV1, BlobWithMetadataV2, BlobsBundle, BlobsBundleVersion,
    BlockMergingData, BundleOrder, KzgCommitment, MergeableBundle, MergeableOrder, MergeableOrders,
    MergeableTransaction, Order, SignedBidSubmission, Transactions,
};
use tracing::error;

use crate::{
    api::Api, auctioneer::AuctioneerHandle,
    database::postgres::postgres_db_service::PostgresDatabaseService, housekeeper::CurrentSlotInfo,
};

pub(crate) const MAX_PAYLOAD_LENGTH: usize = 1024 * 1024 * 20; // 20MB

#[derive(Clone)]
pub struct BuilderApi<A: Api> {
    pub local_cache: Arc<LocalCache>,
    pub db: Arc<PostgresDatabaseService>,
    pub curr_slot_info: CurrentSlotInfo,
    pub relay_config: Arc<RelayConfig>,
    /// Subscriber for TopBid updates, SSZ encoded
    pub top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
    pub auctioneer_handle: AuctioneerHandle,
    pub api_provider: Arc<A::ApiProvider>,
}

impl<A: Api> BuilderApi<A> {
    pub fn new(
        local_cache: Arc<LocalCache>,
        db: Arc<PostgresDatabaseService>,
        relay_config: RelayConfig,
        curr_slot_info: CurrentSlotInfo,
        top_bid_tx: tokio::sync::broadcast::Sender<Bytes>,
        auctioneer_handle: AuctioneerHandle,
        api_provider: Arc<A::ApiProvider>,
    ) -> Self {
        Self {
            local_cache,
            db,
            relay_config: Arc::new(relay_config),
            curr_slot_info,
            top_bid_tx,
            auctioneer_handle,
            api_provider,
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
}

#[derive(thiserror::Error, Debug)]
pub enum OrderValidationError {
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
    let block_blobs_bundles = payload.blobs_bundle();
    let blob_versioned_hashes: Vec<_> =
        block_blobs_bundles.commitments().iter().map(|c| calculate_versioned_hash(*c)).collect();
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
    let version = bundle.version();
    blob_versioned_hashes
        .into_iter()
        .zip(bundle.iter_blobs())
        .map(|(versioned_hash, (blob, commitment, proofs))| match version {
            BlobsBundleVersion::V1 => (
                versioned_hash,
                BlobWithMetadata::V1(BlobWithMetadataV1 {
                    commitment: *commitment,
                    proof: proofs[0],
                    blob: blob.clone(),
                }),
            ),
            BlobsBundleVersion::V2 => (
                versioned_hash,
                BlobWithMetadata::V2(BlobWithMetadataV2 {
                    commitment: *commitment,
                    proofs: proofs.to_vec(),
                    blob: blob.clone(),
                }),
            ),
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
