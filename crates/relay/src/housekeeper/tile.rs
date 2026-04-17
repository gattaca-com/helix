use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use alloy_primitives::B256;
use flux::{
    spine::SpineProducers,
    tile::{Tile, TileName},
};
use flux_utils::SharedVector;
use helix_common::{
    CurrentSlotInfo, InclusionListConfig, PayloadAttributesUpdate, PrimevConfig, ProposerDuty,
    RelayConfig, SlotDuties, ValidatorSummary,
    api::builder_api::{BuilderGetValidatorsResponseEntry, InclusionListWithMetadata},
    beacon::{
        MultiBeaconClient,
        types::{BeaconResponse, HeadEventData, PayloadAttributesEvent, SyncStatus},
    },
    chain_info::ChainInfo,
    http::client::{HttpClient, PendingResponse, SseStream},
    local_cache::LocalCache,
};
use helix_types::Slot;
use tracing::{debug, error, info, warn};

use crate::{
    DbHandle, HelixSpine,
    housekeeper::{
        chain_head::ChainHead,
        duties::{DutiesFetchState, process_duties},
        inclusion_list_service::{IL_CUTOFF, IlFetchState},
        payload_attrs::process_payload_attributes,
        primev_service::{
            PRIMEV_BUILDER_ID, PrimevBuildersFetch, PrimevValidatorsFetch,
            build_primev_builder_configs,
        },
    },
    network::RelayNetworkManager,
    spine::messages::SlotMsg,
};

const KNOWN_VALIDATORS_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
const SYNC_STATUS_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(4);

/// Slot event stored in SharedVector and broadcast via spine.
#[derive(Clone, Debug)]
pub struct SlotUpdate {
    pub bid_slot: Slot,
    pub registration_data: Option<BuilderGetValidatorsResponseEntry>,
    pub payload_attributes: Vec<PayloadAttributesUpdate>,
    pub il: Option<InclusionListWithMetadata>,
}

pub struct HousekeeperTile {
    // Internal state
    chain_head: ChainHead,

    // Data sources
    http_client: HttpClient,
    beacon_client: Arc<MultiBeaconClient>,
    head_sse: Vec<SseStream>,
    payload_attr_sse: Vec<SseStream>,
    primev_config: Option<PrimevConfig>,
    il_config: Option<InclusionListConfig>,

    // Output
    slot_events: Arc<SharedVector<SlotUpdate>>,
    local_cache: Arc<LocalCache>,
    curr_slot_info: CurrentSlotInfo,
    relay_network_api: Arc<RelayNetworkManager>,
    db: DbHandle,

    known_payload_attributes: HashMap<(B256, Slot), PayloadAttributesUpdate>,
    duties: Vec<ProposerDuty>,

    // In-flight fetch state machines
    duties_fetch: Option<DutiesFetchState>,
    primev_builders_fetch: Option<PrimevBuildersFetch>,
    primev_validators_fetch: Option<PrimevValidatorsFetch>,
    il_fetch: Option<IlFetchState>,
    pending_il: Option<helix_common::api::builder_api::InclusionListWithMetadata>,
    il_consensus_rx: Option<
        tokio::sync::oneshot::Receiver<Option<helix_common::api::builder_api::InclusionList>>,
    >,
    known_validators_fetch: Option<(Instant, PendingResponse)>,
    /// None = disabled (not a registration instance).
    known_validators_next_refresh: Option<Instant>,

    // Sync status polling: one in-flight PendingResponse per beacon client (indexed).
    sync_check_pending: Vec<(usize, PendingResponse)>,
    sync_check_next: Instant,
}

impl HousekeeperTile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: DbHandle,
        beacon_client: Arc<MultiBeaconClient>,
        local_cache: Arc<LocalCache>,
        config: &RelayConfig,
        chain_info: Arc<ChainInfo>,
        slot_events: Arc<SharedVector<SlotUpdate>>,
        relay_network_api: Arc<RelayNetworkManager>,
    ) -> (Self, CurrentSlotInfo) {
        let curr_slot_info = CurrentSlotInfo::new();

        let chain_head = ChainHead::new(chain_info);

        let http_client = HttpClient::new().expect("http client init");
        let head_sse = beacon_client
            .beacon_clients
            .iter()
            .map(|c| {
                let mut url = c.config.url.join("/eth/v1/events").unwrap();
                url.set_query(Some("topics=head"));
                http_client.sse_stream(url)
            })
            .collect();
        let payload_attr_sse = beacon_client
            .beacon_clients
            .iter()
            .map(|c| {
                let mut url = c.config.url.join("/eth/v1/events").unwrap();
                url.set_query(Some("topics=payload_attributes"));
                http_client.sse_stream(url)
            })
            .collect();

        let tile = Self {
            chain_head,
            http_client,
            beacon_client,
            head_sse,
            payload_attr_sse,
            primev_config: config.primev_config.clone(),
            il_config: config.inclusion_list.clone(),
            slot_events,
            local_cache,
            curr_slot_info: curr_slot_info.clone(),
            relay_network_api,
            db,
            known_payload_attributes: HashMap::new(),
            duties: Vec::with_capacity(64),
            duties_fetch: None,
            primev_builders_fetch: None,
            primev_validators_fetch: None,
            il_fetch: None,
            pending_il: None,
            il_consensus_rx: None,
            known_validators_fetch: None,
            known_validators_next_refresh: config.is_registration_instance.then(Instant::now),
            sync_check_pending: Vec::new(),
            sync_check_next: Instant::now(),
        };
        (tile, curr_slot_info)
    }

    fn on_new_slot(&mut self) {
        let slot = self.chain_head.head();
        info!(%slot, "new slot started");
        self.pending_il = None;
        self.il_consensus_rx = None;

        if let Some(cfg) = &self.primev_config {
            self.primev_builders_fetch = PrimevBuildersFetch::new(&self.http_client, cfg);
            self.primev_validators_fetch =
                PrimevValidatorsFetch::new(&self.http_client, cfg, self.duties.clone());
        }
        if let Some(cfg) = &self.il_config {
            let slot = self.chain_head.head();
            let elapsed = self.chain_head.chain_info().duration_into_slot(slot).unwrap_or_default();
            let timeout = IL_CUTOFF.saturating_sub(elapsed);
            self.il_fetch = IlFetchState::start(&self.http_client, cfg, timeout);
            info!("expecting inclusion list for new slot from IL service");
        } else {
            self.chain_head.mark_il_done();
        }

        self.duties_fetch = DutiesFetchState::new(
            &self.http_client,
            &self.beacon_client,
            self.chain_head.epoch().as_u64(),
        );

        let bid_slot = slot + 1;
        for d in &self.duties {
            if d.slot == bid_slot {
                info!(%d.pubkey, "duty for new slot found in cache");
                self.chain_head.mark_duties_done();
                break;
            }
        }
        self.known_payload_attributes.iter().for_each(|((_, s), _)| {
            if *s == bid_slot {
                info!(%bid_slot, "payload attributes for new slot found in cache");
                self.chain_head.mark_payload_attrs_done();
            }
        });
    }
}

impl Tile<HelixSpine> for HousekeeperTile {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        use std::task::Poll;

        // Clock-based new slot detection runs before SSE polling. Both signals
        // (clock tick and head event) fire at the same moment each slot. By checking
        // the clock first, the SSE event arrives as already_seen (NewSlot → Pending)
        // instead of racing into the else branch and skipping fetch startup.
        if self.chain_head.is_new_slot() {
            self.on_new_slot();
        }

        // Poll head SSE. on_new_slot() needs &mut self, so we can't call it inside
        // the loop (iterator holds &mut self.head_sse). Track a bool and call after.
        let mut new_slot_from_sse = false;
        for stream in &mut self.head_sse {
            if let Poll::Ready(ev) = stream.poll() {
                match serde_json::from_str::<HeadEventData>(&ev.data) {
                    Ok(data) => {
                        if self.chain_head.update(data) {
                            new_slot_from_sse = true;
                            break;
                        }
                    }
                    Err(e) => error!(err = %e, "head SSE parse error"),
                }
            }
        }
        if new_slot_from_sse {
            // SSE advanced the slot before the clock fired (beacon slightly ahead,
            // or same iteration). is_new_slot() returned false for this slot.
            self.on_new_slot();
        }
        for stream in &mut self.payload_attr_sse {
            if let Poll::Ready(ev) = stream.poll() {
                match serde_json::from_str::<PayloadAttributesEvent>(&ev.data) {
                    Ok(data) => process_payload_attributes(
                        &mut self.chain_head,
                        data,
                        &mut self.known_payload_attributes,
                    ),
                    Err(e) => error!(err = %e, "payload_attributes SSE parse error"),
                }
            }
        }

        // Poll duties fetch
        let duties_result =
            self.duties_fetch.as_mut().and_then(|f| match f.poll(&self.http_client) {
                Poll::Ready(r) => Some(r),
                Poll::Pending => None,
            });
        if let Some(result) = duties_result {
            self.duties_fetch = None;
            match result {
                Ok(proposer_duties) if proposer_duties.is_empty() => {
                    warn!("no proposer duties found");
                }
                Ok(proposer_duties) => {
                    process_duties(&proposer_duties, &self.local_cache, &self.db);
                    self.duties = proposer_duties;
                    self.chain_head.mark_duties_done();
                }
                Err(e) => error!(%e, "failed to fetch proposer duties"),
            }
        }

        // Poll primev builders
        if let Some(state) = &mut self.primev_builders_fetch &&
            let Poll::Ready(builders) = state.poll()
        {
            self.primev_builders_fetch = None;
            if !builders.is_empty() {
                info!(builders = builders.len(), "fetched primev builders");
                let configs = build_primev_builder_configs(builders, &self.local_cache);
                self.local_cache.update_builder_infos(&configs, false);
                self.db.store_builders_info(configs);
            }
        }

        // Poll primev validators
        if let Some(state) = &mut self.primev_validators_fetch &&
            let Poll::Ready(validators) = state.poll()
        {
            self.primev_validators_fetch = None;
            if !validators.is_empty() {
                info!(validators = validators.len(), "fetched primev validators");
                self.local_cache.update_primev_proposers(&validators);
            }
        }

        // Poll inclusion list fetch
        let il_result = self.il_fetch.as_mut().and_then(|f| match f.poll(&self.http_client) {
            Poll::Ready(r) => Some(r),
            Poll::Pending => None,
        });
        if let Some(il_opt) = il_result {
            self.il_fetch = None;
            match il_opt {
                Some(il) => {
                    let head = self.chain_head.head();
                    match InclusionListWithMetadata::try_from(il.clone()) {
                        Ok(il_meta) => {
                            info!(txs = il_meta.txs.len(), "fetched inclusion list");
                            if let Some(parent_hash) = self.chain_head.block() &&
                                let Some(duty) = self.duties.iter().find(|d| d.slot == head)
                            {
                                self.local_cache.update_current_inclusion_list(
                                    il_meta.clone(),
                                    (head.as_u64(), duty.pubkey, parent_hash),
                                );
                                self.db.save_inclusion_list(
                                    il_meta.clone(),
                                    head.as_u64(),
                                    parent_hash,
                                    duty.pubkey,
                                );
                            }
                            self.pending_il = Some(il_meta);
                        }
                        Err(err) => warn!(%err, %head, "could not decode inclusion list RLP"),
                    }
                    self.chain_head.mark_il_done();
                    self.il_consensus_rx =
                        self.relay_network_api.try_share_inclusion_list(head.as_u64(), il);
                }
                None => {
                    warn!("inclusion list fetch timed out");
                    self.chain_head.mark_il_done();
                }
            }
        }

        // Poll known validators refresh (registration instances only)
        if let Some((start, ref mut req)) = self.known_validators_fetch {
            match req.poll_json::<BeaconResponse<Vec<ValidatorSummary>>>() {
                Poll::Pending => {}
                Poll::Ready(result) => {
                    self.known_validators_fetch = None;
                    match result {
                        Ok(resp) => {
                            debug!(validators = resp.data.len(), duration = ?start.elapsed(), "refreshed known validators");
                            self.db.set_known_validators(resp.data);
                        }
                        Err(err) => error!(%err, "failed to refresh known validators"),
                    }
                }
            }
        }
        if self.known_validators_fetch.is_none() &&
            let Some(next) = self.known_validators_next_refresh &&
            Instant::now() >= next &&
            let Some(c) = self.beacon_client.beacon_clients_by_last_response().next() &&
            let Ok(url) =
                c.config.url.join("eth/v1/beacon/states/head/validators?status=active,pending") &&
            let Ok(req) = self.http_client.get(&url)
        {
            self.known_validators_fetch = Some((Instant::now(), req));
            self.known_validators_next_refresh =
                Some(Instant::now() + KNOWN_VALIDATORS_REFRESH_INTERVAL);
        }

        // Poll in-flight sync status requests.
        if !self.sync_check_pending.is_empty() {
            let mut best: Option<(usize, u64)> = None;
            let pending = std::mem::take(&mut self.sync_check_pending);
            for (idx, mut req) in pending {
                match req.poll_json::<BeaconResponse<SyncStatus>>() {
                    Poll::Pending => self.sync_check_pending.push((idx, req)),
                    Poll::Ready(Ok(resp)) => {
                        let slot = resp.data.head_slot.as_u64();
                        if best.as_ref().is_none_or(|(_, s)| slot > *s) {
                            best = Some((idx, slot));
                        }
                    }
                    Poll::Ready(Err(e)) => warn!(%e, idx, "sync status fetch failed"),
                }
            }
            if self.sync_check_pending.is_empty() &&
                let Some((best_idx, _)) = best
            {
                self.beacon_client.best_index.store(best_idx, Ordering::Relaxed);
            }
        }
        // Start a new sync status batch when the previous one is done and the timer fires.
        if self.sync_check_pending.is_empty() && Instant::now() >= self.sync_check_next {
            for (idx, client) in self.beacon_client.beacon_clients.iter().enumerate() {
                if let Ok(url) = client.config.url.join("eth/v1/node/syncing") &&
                    let Ok(req) = self.http_client.get(&url)
                {
                    self.sync_check_pending.push((idx, req));
                }
            }
            self.sync_check_next = Instant::now() + SYNC_STATUS_CHECK_INTERVAL;
        }

        // Poll for consensus IL result from multi-relay service.
        if let Some(rx) = &mut self.il_consensus_rx {
            match rx.try_recv() {
                Ok(Some(il)) => {
                    match InclusionListWithMetadata::try_from(il) {
                        Ok(il_meta) => self.pending_il = Some(il_meta),
                        Err(e) => warn!(%e, "could not decode consensus IL"),
                    }
                    self.il_consensus_rx = None;
                }
                Ok(None) => {
                    self.il_consensus_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    warn!("IL consensus channel closed before result");
                    self.il_consensus_rx = None;
                }
            }
        }

        if self.chain_head.is_ready() {
            send_slot_event(
                adapter,
                &mut self.chain_head,
                &self.local_cache,
                &self.curr_slot_info,
                &self.slot_events,
                &self.known_payload_attributes,
                self.pending_il.take(),
            );
        }
    }

    fn name(&self) -> TileName {
        TileName::from_str_truncate("housekeeper")
    }
}

fn send_slot_event(
    adapter: &mut flux::spine::SpineAdapter<HelixSpine>,
    chain_head: &mut ChainHead,
    local_cache: &LocalCache,
    curr_slot_info: &CurrentSlotInfo,
    slot_events: &SharedVector<SlotUpdate>,
    known_payload_attributes: &HashMap<(B256, Slot), PayloadAttributesUpdate>,
    il: Option<InclusionListWithMetadata>,
) {
    let slot = chain_head.head();
    let bid_slot = slot + 1;

    let mut new_duties = local_cache.get_proposer_duties();
    for duty in new_duties.iter_mut() {
        let pubkey = &duty.entry.registration.message.pubkey;
        if local_cache.is_primev_proposer(pubkey) {
            info!(%pubkey, "Primev proposer duty found");
            let tb = duty.entry.preferences.trusted_builders.get_or_insert_with(Vec::new);
            if !tb.iter().any(|b| b == PRIMEV_BUILDER_ID) {
                tb.push(PRIMEV_BUILDER_ID.to_string());
            }
        }
    }

    let next_duty = new_duties.iter().find(|d| d.slot.as_u64() == bid_slot.as_u64()).cloned();
    let next_payload_attributes: Vec<PayloadAttributesUpdate> = known_payload_attributes
        .iter()
        .filter_map(
            |((_, s), a)| if s.as_u64() == bid_slot.as_u64() { Some(a.clone()) } else { None },
        )
        .collect();

    info!(
        bid_slot = %bid_slot,
        duties_available = !new_duties.is_empty(),
        has_next_duty = next_duty.is_some(),
        has_payload_attrs = !next_payload_attributes.is_empty(),
        "sending slot event to auctioneer",
    );

    let update = SlotDuties { slot, new_duties: Some(new_duties), next_duty: next_duty.clone() };
    curr_slot_info.handle_new_slot(update, chain_head.chain_info());
    for attr in &next_payload_attributes {
        curr_slot_info.handle_new_payload_attributes(attr.clone());
    }

    let ix = slot_events.push(SlotUpdate {
        bid_slot,
        registration_data: next_duty,
        payload_attributes: next_payload_attributes,
        il,
    });
    adapter.producers.produce(SlotMsg { ix });

    chain_head.sent();
}
