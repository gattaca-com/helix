//! The building role: builds a block for the next slot and submits it to the
//! relay. Shares the embedded ethrex node with the other roles.

mod assemble;
mod keys;
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
use tracing::{error, info, warn};
pub use watcher::run as watch_slots;

use crate::{building::slot::SlotContext, config::BuildingConfig};

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

    while let Some(slot) = contexts.recv().await {
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
