//! The building role: builds a block for the next slot and submits it to the
//! relay. Shares the embedded ethrex node with the other roles.

mod assemble;
mod keys;
mod schedule;
mod slot;
mod submit;
mod watcher;

use std::{sync::Arc, time::Duration};

use alloy_signer_local::PrivateKeySigner;
use ethrex_blockchain::Blockchain;
use ethrex_storage::Store;
use helix_common::signing::RelaySigningContext;
pub use keys::BuildingKeys;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
pub use watcher::run as watch_slots;

use crate::{
    building::{
        schedule::{Attempt, BestBid},
        slot::SlotContext,
    },
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
    contexts: mpsc::Receiver<SlotContext>,
) {
    let lead_ms = config.build_lead_ms;
    // A slot cannot outlive itself: without a closing answer from the relay the
    // loop still has to end.
    let budget = Duration::from_secs(signing.chain_info.seconds_per_slot());
    let submitter =
        Arc::new(submit::Submitter::new(&config.relay_url, config.api_key.clone(), signing));
    let best = Arc::new(std::sync::Mutex::new(BestBid::default()));

    let attempt = move |slot: SlotContext| {
        let (config, store, blockchain, signer, submitter, best) = (
            config.clone(),
            store.clone(),
            blockchain.clone(),
            payout_signer.clone(),
            submitter.clone(),
            best.clone(),
        );
        async move {
            let build_slot = slot.clone();
            // Building is CPU-bound and must not stall the runtime.
            let built = tokio::task::spawn_blocking(move || {
                assemble::build(&store, &blockchain, &build_slot, &config, &signer, chain_id)
            })
            .await;

            let built = match built {
                Ok(Ok(built)) => built,
                Ok(Err(e)) => {
                    warn!(slot = slot.slot, err = %e, "skipping slot");
                    return Attempt::Continue;
                }
                Err(e) => {
                    error!(slot = slot.slot, err = %e, "build task panicked");
                    return Attempt::Continue;
                }
            };

            {
                let mut best = best.lock().expect("bid tracker mutex");
                best.prune(slot.slot);
                if !best.improves(slot.slot, slot.parent_hash, built.value) {
                    debug!(slot = slot.slot, value = %built.value, "not an improvement");
                    return Attempt::Continue;
                }
            }

            let bid = match submitter.sign(&built, &slot) {
                Ok(bid) => bid,
                Err(e) => {
                    warn!(slot = slot.slot, err = %e, "cannot sign the block");
                    return Attempt::Continue;
                }
            };

            match submitter.submit(&bid).await {
                Ok(()) => {
                    info!(
                        slot = slot.slot,
                        block_hash = %bid.message().block_hash,
                        txs = built.block.body.transactions.len(),
                        value = %built.value,
                        "submitted a block",
                    );
                    Attempt::Continue
                }
                Err(e) if e.is_slot_closed() => {
                    info!(slot = slot.slot, "the relay has moved to the next slot");
                    Attempt::SlotClosed
                }
                // The relay's reason is how an operator learns the blocks are bad.
                Err(e) => {
                    warn!(slot = slot.slot, err = %e, "the relay refused the block");
                    Attempt::Continue
                }
            }
        }
    };

    schedule::drive(contexts, lead_ms, budget, now_ms, attempt).await;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the unix epoch")
        .as_millis() as u64
}
