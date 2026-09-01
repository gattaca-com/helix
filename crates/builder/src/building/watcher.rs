use std::time::Duration;

use eyre::Context;
use futures::StreamExt;
use helix_common::{
    api::{PATH_BUILDER_API, PATH_GET_VALIDATORS, builder_api::BuilderGetValidatorsResponse},
    beacon::types::PayloadAttributesEvent,
};
use reqwest_eventsource::{Event, EventSource};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{
    building::slot::{SlotContext, SlotTracker},
    config::BuildingConfig,
};

/// Proposer duties cover the current and next epoch, so this only has to beat
/// an epoch boundary.
const DUTIES_REFRESH: Duration = Duration::from_secs(12);

pub async fn fetch_duties(
    http: &reqwest::Client,
    relay_url: &str,
) -> eyre::Result<Vec<BuilderGetValidatorsResponse>> {
    let url = format!("{}{PATH_BUILDER_API}{PATH_GET_VALIDATORS}", relay_url.trim_end_matches('/'));
    let response = http.get(&url).send().await.wrap_err("get_validators request failed")?;
    let status = response.status();
    if !status.is_success() {
        eyre::bail!("get_validators returned {status}");
    }
    response.json().await.wrap_err("get_validators returned malformed JSON")
}

/// Merges the beacon node's `payload_attributes` events with the relay's
/// proposer duties, and publishes one [`SlotContext`] per slot worth building.
pub async fn run(config: BuildingConfig, contexts: mpsc::Sender<SlotContext>) {
    let http = reqwest::Client::new();
    let mut tracker = SlotTracker::default();

    let events_url = format!(
        "{}/eth/v1/events?topics=payload_attributes",
        config.beacon_url.trim_end_matches('/')
    );
    // EventSource reconnects on its own; a replayed event is discarded by the
    // tracker's staleness check.
    let mut events = EventSource::get(&events_url);

    let mut refresh = tokio::time::interval(DUTIES_REFRESH);

    loop {
        tokio::select! {
            _ = refresh.tick() => match fetch_duties(&http, &config.relay_url).await {
                Ok(duties) => {
                    debug!(count = duties.len(), "refreshed proposer duties");
                    tracker.on_duties(duties);
                }
                Err(e) => warn!(err = %e, "failed to refresh proposer duties"),
            },
            Some(event) = events.next() => match event {
                Ok(Event::Open) => info!(url = %events_url, "subscribed to payload_attributes"),
                Ok(Event::Message(message)) => {
                    match serde_json::from_str::<PayloadAttributesEvent>(&message.data) {
                        Ok(event) => {
                            if let Some(context) = tracker.on_payload_attributes(event) {
                                info!(
                                    slot = context.slot,
                                    parent_hash = %context.parent_hash,
                                    gas_limit = context.registered_gas_limit,
                                    "building for slot",
                                );
                                if contexts.send(context).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => warn!(err = %e, "malformed payload_attributes event"),
                    }
                }
                Err(e) => warn!(err = %e, "payload_attributes stream error"),
            },
        }
    }
}
