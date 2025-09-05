use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use helix_common::{
    api::builder_api::{InclusionList, InclusionListWithMetadata},
    chain_info::ChainInfo,
    InclusionListConfig,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{BlsPublicKeyBytes, Slot};
use tracing::{error, info, warn};

use crate::inclusion_list::http_fetcher::HttpInclusionListFetcher;

const MISSING_INCLUSION_LIST_CUTOFF: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub struct InclusionListService<DB: DatabaseService, A: Auctioneer> {
    db: Arc<DB>,
    auctioneer: Arc<A>,
    http_il_fetcher: HttpInclusionListFetcher,
    chain_info: Arc<ChainInfo>,
}

impl<DB: DatabaseService, A: Auctioneer> InclusionListService<DB, A> {
    pub fn new(
        db: Arc<DB>,
        auctioneer: Arc<A>,
        config: InclusionListConfig,
        chain_info: Arc<ChainInfo>,
    ) -> Self {
        let http_il_fetcher = HttpInclusionListFetcher::new(config);

        Self { db, auctioneer, http_il_fetcher, chain_info }
    }

    /// Fetch and persist inclusion list for this slot.
    pub async fn handle_inclusion_list_for_slot(
        &self,
        parent_hash: Option<B256>,
        pub_key: BlsPublicKeyBytes,
        slot: u64,
    ) {
        let Some(parent_hash) = parent_hash else {
            info!("No inclusion list for this slot because we missed the new slot head event and have no block hash");
            return;
        };

        let Some(inclusion_list) = self.fetch_inclusion_list_or_timeout(slot).await else {
            return;
        };

        let inclusion_list = match InclusionListWithMetadata::try_from(inclusion_list) {
            Ok(list) => list,
            Err(err) => {
                warn!(head_slot = slot, "Could not decode inclusion list RLP bytes. Error:{}", err);
                return;
            }
        };

        self.auctioneer.update_current_inclusion_list(
            inclusion_list.clone(),
            (slot, pub_key.clone(), parent_hash),
        );

        match self.db.save_inclusion_list(&inclusion_list, slot, &parent_hash, &pub_key).await {
            Ok(_) => {
                info!(head_slot = slot, "Saved inclusion list to postgres");
            }
            Err(err) => {
                error!(
                    head_slot = slot,
                    "Could not save inclusion list to postgres. Error: {:?}", err
                );
            }
        }
    }

    async fn fetch_inclusion_list_or_timeout(&self, slot: u64) -> Option<InclusionList> {
        tokio::select! {
            inclusion_list = self.http_il_fetcher.fetch_inclusion_list_with_retry(slot) => {
                Some(inclusion_list)
            }
            _ = tokio::time::sleep(self.time_to_missing_inclusion_list_cutoff(slot.into())) => {
                warn!(head_slot = slot,
                    "No inclusion list for this slot. We have reached the {}s cutoff and have not been able to source one.",
                    MISSING_INCLUSION_LIST_CUTOFF.as_secs()
                );
                None
            }
        }
    }

    fn time_to_missing_inclusion_list_cutoff(&self, slot: Slot) -> Duration {
        self.chain_info
            .duration_into_slot(slot)
            .and_then(|time_into_slot| MISSING_INCLUSION_LIST_CUTOFF.checked_sub(time_into_slot))
            .unwrap_or(Duration::ZERO)
    }
}
