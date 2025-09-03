use helix_common::metrics::HYDRATION_CACHE_HITS;
use helix_types::{DehydratedBidSubmission, HydrationCache, HydrationError, SignedBidSubmission};
use tokio::sync::oneshot;
use tracing::error;

pub fn spawn_hydration_task(
    mut hydration_tx: tokio::sync::mpsc::Receiver<(
        u64,
        DehydratedBidSubmission,
        oneshot::Sender<Result<SignedBidSubmission, HydrationError>>,
    )>,
) {
    tokio::task::spawn_blocking(|| async move {
        let mut last_slot = 0;
        let mut cache = HydrationCache::new();

        while let Some((slot, dehydrated_bid_submission, sender)) = hydration_tx.recv().await {
            if slot > last_slot {
                last_slot = slot;
                cache.clear();
            }

            let result = match dehydrated_bid_submission.hydrate(&mut cache) {
                Ok((result, tx_cache_hits, blob_cache_hits)) => {
                    HYDRATION_CACHE_HITS
                        .with_label_values(&["transaction"])
                        .inc_by(tx_cache_hits as u64);
                    HYDRATION_CACHE_HITS
                        .with_label_values(&["blob"])
                        .inc_by(blob_cache_hits as u64);

                    Ok(result)
                }
                Err(err) => Err(err),
            };

            if let Err(err) = sender.send(result) {
                error!(?err, "failed to send back hydrated payload");
            };
        }

        error!("hydration task exited");
    });
}
