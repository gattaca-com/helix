use std::{sync::Arc, time::Duration};

use alloy_primitives::B256;
use helix_common::{
    InclusionListConfig,
    api::builder_api::{InclusionList, InclusionListWithMetadata},
    chain_info::ChainInfo,
    local_cache::LocalCache,
};
use helix_types::{BlsPublicKeyBytes, Slot};
use tracing::{info, warn};

use crate::{
    DbService, auctioneer::Event,
    housekeeper::inclusion_list::http_fetcher::HttpInclusionListFetcher,
    network::RelayNetworkManager,
};

const MISSING_INCLUSION_LIST_CUTOFF: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub struct InclusionListService {
    db: DbService,
    local_cache: Arc<LocalCache>,
    http_il_fetcher: HttpInclusionListFetcher,
    chain_info: Arc<ChainInfo>,
    event_tx: crossbeam_channel::Sender<Event>,
    network_api: Arc<RelayNetworkManager>,
}

impl InclusionListService {
    pub fn new(
        db: DbService,
        local_cache: Arc<LocalCache>,
        config: InclusionListConfig,
        chain_info: Arc<ChainInfo>,
        event_tx: crossbeam_channel::Sender<Event>,
        network_api: Arc<RelayNetworkManager>,
    ) -> Self {
        let http_il_fetcher = HttpInclusionListFetcher::new(config);

        Self { db, local_cache, event_tx, http_il_fetcher, chain_info, network_api }
    }

    /// Fetch and persist inclusion list for this slot.
    pub async fn handle_inclusion_list_for_slot(
        &self,
        parent_hash: Option<B256>,
        pub_key: BlsPublicKeyBytes,
        head_slot: u64,
    ) {
        let Some(parent_hash) = parent_hash else {
            info!(
                "No inclusion list for this slot because we missed the new slot head event and have no block hash"
            );
            return;
        };

        let Some(inclusion_list) = self.fetch_inclusion_list_or_timeout(head_slot).await else {
            return;
        };

        let inclusion_list = match InclusionListWithMetadata::try_from(inclusion_list) {
            Ok(list) => list,
            Err(err) => {
                warn!(
                    head_slot = head_slot,
                    "Could not decode inclusion list RLP bytes. Error:{}", err
                );
                return;
            }
        };

        self.local_cache.update_current_inclusion_list(
            inclusion_list.clone(),
            (head_slot, pub_key, parent_hash),
        );

        let _ = self.event_tx.try_send(Event::SlotData {
            bid_slot: (head_slot + 1).into(),
            registration_data: None,
            payload_attributes: None,
            il: Some(inclusion_list.clone()),
        });

        self.db.save_inclusion_list(inclusion_list, head_slot, parent_hash, pub_key);
    }

    async fn fetch_inclusion_list_or_timeout(&self, slot: u64) -> Option<InclusionList> {
        tokio::select! {
            inclusion_list = self.fetch_inclusion_list_and_share_with_peers(slot) => {
                inclusion_list
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

    async fn fetch_inclusion_list_and_share_with_peers(&self, slot: u64) -> Option<InclusionList> {
        let inclusion_list = self.http_il_fetcher.fetch_inclusion_list_with_retry(slot).await;
        self.network_api.share_inclusion_list(slot, inclusion_list).await
    }

    fn time_to_missing_inclusion_list_cutoff(&self, slot: Slot) -> Duration {
        self.chain_info
            .duration_into_slot(slot)
            .and_then(|time_into_slot| MISSING_INCLUSION_LIST_CUTOFF.checked_sub(time_into_slot))
            .unwrap_or(Duration::ZERO)
    }
}
