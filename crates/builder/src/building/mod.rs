//! The building role: builds a block for the next slot and submits it to the
//! relay. Shares the embedded ethrex node with the other roles.

mod assemble;
mod keys;
mod schedule;
mod slot;
mod submit;
mod watcher;

use std::sync::Arc;

use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::Blockchain;
use ethrex_storage::Store;
use helix_common::signing::RelaySigningContext;
pub use keys::BuildingKeys;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
pub use watcher::run as watch_slots;

use crate::{
    building::{schedule::BestBid, slot::SlotContext},
    config::BuildingConfig,
};

/// Reads the network's spec and genesis from the beacon node, so the builder
/// domain is never a hardcoded per-network constant.
pub async fn signing_context(beacon_url: &str) -> eyre::Result<RelaySigningContext> {
    let url = beacon_url
        .parse()
        .map_err(|e| eyre::eyre!("building config: beacon_url is not a URL: {e}"))?;
    let chain_info =
        helix_common::beacon::BeaconClient::new(helix_common::config::BeaconClientConfig { url })
            .get_chain_info()
            .await
            .map_err(|e| eyre::eyre!("cannot read the chain spec from the beacon node: {e:?}"))?;
    Ok(RelaySigningContext::new(BuildingKeys::load()?.bls, Arc::new(chain_info)))
}

/// Builds and submits a block for every slot the watcher publishes.
pub async fn build_blocks(
    config: BuildingConfig,
    store: Store,
    blockchain: Arc<Blockchain>,
    payout_signer: PrivateKeySigner,
    signing: RelaySigningContext,
    chain_id: u64,
    mut contexts: mpsc::Receiver<SlotContext>,
) {
    let submitter = submit::Submitter::new(&config.relay_url, config.api_key.clone(), signing);
    let mut best = BestBid::default();

    while let Some(slot) = contexts.recv().await {
        best.prune(slot.slot);

        // Each delay is measured from the same instant, so they must not be
        // slept end to end.
        let base = tokio::time::Instant::now();
        for delay in schedule::delays(slot.timestamp, &config.submit_offsets_ms, now_ms()) {
            tokio::time::sleep_until(base + delay).await;

            let (build_config, store, blockchain, signer, build_slot) = (
                config.clone(),
                store.clone(),
                blockchain.clone(),
                payout_signer.clone(),
                slot.clone(),
            );
            // Building is CPU-bound and must not stall the runtime.
            let built = tokio::task::spawn_blocking(move || {
                assemble::build(&store, &blockchain, &build_slot, &build_config, &signer, chain_id)
            })
            .await;

            let built = match built {
                Ok(Ok(built)) => built,
                Ok(Err(e)) => {
                    warn!(slot = slot.slot, err = %e, "skipping slot");
                    continue;
                }
                Err(e) => {
                    error!(slot = slot.slot, err = %e, "build task panicked");
                    continue;
                }
            };

            if !best.improves(slot.slot, slot.parent_hash, built.value) {
                debug!(slot = slot.slot, value = %built.value, "not an improvement");
                continue;
            }

            let submission = match submitter.sign(&built, &slot) {
                Ok(submission) => submission,
                Err(e) => {
                    warn!(slot = slot.slot, err = %e, "cannot sign the block");
                    continue;
                }
            };

            match submitter.submit(&submission).await {
                Ok(()) => info!(
                    slot = slot.slot,
                    block_hash = %submission.message.block_hash,
                    txs = built.block.body.transactions.len(),
                    value = %built.value,
                    "submitted a block",
                ),
                // The relay's reason is how an operator learns the blocks are bad.
                Err(e) => warn!(slot = slot.slot, err = %e, "the relay refused the block"),
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the unix epoch")
        .as_millis() as u64
}
