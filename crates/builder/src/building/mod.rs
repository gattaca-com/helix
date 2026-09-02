//! The building role: builds a block for the next slot and submits it to the
//! relay. Shares the embedded ethrex node with the other roles.

mod assemble;
mod keys;
mod slot;
mod watcher;

use std::sync::Arc;

use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::Blockchain;
use ethrex_storage::Store;
pub use keys::BuildingKeys;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
pub use watcher::run as watch_slots;

use crate::{building::slot::SlotContext, config::BuildingConfig};

/// Builds a block for every slot the watcher publishes.
pub async fn build_blocks(
    config: BuildingConfig,
    store: Store,
    blockchain: Arc<Blockchain>,
    payout_signer: PrivateKeySigner,
    chain_id: u64,
    mut contexts: mpsc::Receiver<SlotContext>,
) {
    while let Some(slot) = contexts.recv().await {
        let (config, store, blockchain, signer) =
            (config.clone(), store.clone(), blockchain.clone(), payout_signer.clone());
        // Building is CPU-bound and must not stall the runtime.
        let built = tokio::task::spawn_blocking(move || {
            assemble::build(&store, &blockchain, &slot, &config, &signer, chain_id)
        })
        .await;

        match built {
            Ok(Ok(block)) => info!(
                number = block.block.header.number,
                txs = block.block.body.transactions.len(),
                gas_used = block.block.header.gas_used,
                value = %block.value,
                "built a block",
            ),
            // Step 4 submits; until then a built block is only reported.
            Ok(Err(e)) => warn!(err = %e, "skipping slot"),
            Err(e) => error!(err = %e, "build task panicked"),
        }
    }
}
