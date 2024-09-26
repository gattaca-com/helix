use std::{
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension,
};
use ethereum_consensus::phase0::mainnet::SLOTS_PER_EPOCH;
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry,
        constraints_api::{GetConstraintsParams, GetPreconferParams, SignedPreconferElection},
    },
    chain_info::ChainInfo,
    traces::constraints_api::GetGatewayTrace,
};
use helix_datastore::constraints::ConstraintsAuctioneer;
use helix_housekeeper::{ChainUpdate, SlotUpdate};
use tokio::sync::{
    mpsc,
    mpsc::{error::SendError, Sender},
};
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::constraints::error::ConstraintsApiError;

pub(crate) const MAX_GATEWAY_ELECTION_SIZE: usize = 1024 * 1024; // TODO: this should be a fixed size that we calc
pub(crate) const MAX_SET_CONSTRAINTS_SIZE: usize = 1024 * 1024; // TODO: this should be a fixed size that we calc

/// Information about the current head slot and next elected gateway.
#[derive(Clone, Default)]
struct SlotInfo {
    pub slot: u64,
    pub next_elected_preconfer: Option<SignedPreconferElection>,
}

#[derive(Clone)]
pub struct ConstraintsApi<A: ConstraintsAuctioneer> {
    auctioneer: Arc<A>,
    chain_info: Arc<ChainInfo>,
    proposer_duties: Arc<RwLock<Vec<BuilderGetValidatorsResponseEntry>>>,
    curr_slot_info: Arc<RwLock<SlotInfo>>,
}

impl<A> ConstraintsApi<A>
where
    A: ConstraintsAuctioneer + 'static,
{
    pub fn new(auctioneer: Arc<A>, chain_info: Arc<ChainInfo>, slot_update_subscription: Sender<Sender<ChainUpdate>>) -> Self {
        let api = Self {
            auctioneer,
            chain_info,
            proposer_duties: Arc::new(Default::default()),
            curr_slot_info: Arc::new(RwLock::new(SlotInfo::default())),
        };

        // Spin up the housekeep task
        let api_clone = api.clone();
        tokio::spawn(async move {
            if let Err(error) = api_clone.housekeep(slot_update_subscription).await {
                error!(
                    %error,
                    "ConstraintsApi. housekeep task encountered an error",
                );
            }
        });

        api
    }

    /// Returns the constraints for the current slot
    pub async fn get_constraints(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        Path(GetConstraintsParams { slot }): Path<GetConstraintsParams>,
    ) -> Result<impl IntoResponse, ConstraintsApiError> {
        match api.auctioneer.get_constraints(slot).await? {
            Some(constraints) => {
                let constraints_bytes = serde_json::to_vec(&constraints)?;
                Ok(Response::builder().status(StatusCode::OK).body(axum::body::Body::from(constraints_bytes)).unwrap().into_response())
            }
            None => Ok(StatusCode::NO_CONTENT.into_response()),
        }
    }

    /// Returns the preconfer for the given slot. If no elected preconfer is found, it returns an error.
    pub async fn get_preconfer(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        Path(GetPreconferParams { slot }): Path<GetPreconferParams>,
    ) -> Result<axum::Json<SignedPreconferElection>, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = GetGatewayTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;
        debug!(
            request_id = %request_id,
            event = "get_preconfer",
            head_slot = head_slot,
            request_ts = trace.receive,
            request_slot = %slot,
        );

        if slot <= head_slot {
            warn!(%request_id, "request for past slot");
            return Err(ConstraintsApiError::RequestForPastSlot { request_slot: slot, head_slot });
        }

        match api.auctioneer.get_elected_gateway(slot).await {
            Ok(Some(elected_gateway)) => {
                trace.gateway_fetched = get_nanos_timestamp()?;
                debug!(%request_id, ?elected_gateway, ?trace, "found elected gateway");
                return Ok(axum::Json(elected_gateway));
            }
            Ok(None) => {
                warn!(%request_id, "no gateway found for request");
                Err(ConstraintsApiError::NoPreconferFoundForSlot { slot })
            }
            Err(err) => {
                warn!(%request_id, ?err, "no gateway found for request");
                Err(ConstraintsApiError::NoPreconferFoundForSlot { slot })
            }
        }
    }

    /// Returns the preconfer for the given slot. If no elected preconfer is found, it returns an error.
    pub async fn get_preconfers_for_epoch(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
    ) -> Result<axum::Json<Vec<SignedPreconferElection>>, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = GetGatewayTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;
        debug!(
            request_id = %request_id,
            event = "get_preconfers_for_epoch",
            head_slot = head_slot,
            request_ts = trace.receive,
        );

        let slots_for_epoch = get_remaining_slots_for_current_and_next_n_epochs(head_slot + 1, 1);
        let mut preconfers = vec![];
        for slot in slots_for_epoch {
            match api.auctioneer.get_elected_gateway(slot).await {
                Ok(Some(elected_gateway)) => {
                    preconfers.push(elected_gateway);
                }
                _ => {}
            }
        }

        trace.gateway_fetched = get_nanos_timestamp()?;
        debug!(%request_id, num_preconfers_fetched = preconfers.len(), ?trace, "found elected gateway");

        Ok(axum::Json(preconfers))
    }
}

// STATE SYNC
impl<A> ConstraintsApi<A>
where
    A: ConstraintsAuctioneer + 'static,
{
    /// Subscribes to slot head updater.
    /// Updates the current slot and next proposer duty.
    pub async fn housekeep(&self, slot_update_subscription: Sender<Sender<ChainUpdate>>) -> Result<(), SendError<Sender<ChainUpdate>>> {
        let (tx, mut rx) = mpsc::channel(20);
        slot_update_subscription.send(tx).await?;

        while let Some(slot_update) = rx.recv().await {
            match slot_update {
                ChainUpdate::SlotUpdate(slot_update) => {
                    self.handle_new_slot(slot_update).await;
                }
                ChainUpdate::PayloadAttributesUpdate(_) => {}
            }
        }

        Ok(())
    }

    /// Handle a new slot update.
    /// Updates the next proposer duty for the new slot.
    async fn handle_new_slot(&self, slot_update: SlotUpdate) {
        let epoch = slot_update.slot / SLOTS_PER_EPOCH;
        debug!(
            epoch = epoch,
            slot = slot_update.slot,
            slot_start_next_epoch = (epoch + 1) * SLOTS_PER_EPOCH,
            next_proposer_duty = ?slot_update.next_duty,
            "Updated head slot",
        );
        let next_slot = slot_update.slot + 1;

        // Update duties if applicable
        if let Some(new_duties) = slot_update.new_duties {
            *self.proposer_duties.write().unwrap() = new_duties;
        }

        // Fetch elected gateway for the current slot
        let elected_preconfer = match self.auctioneer.get_elected_gateway(next_slot).await {
            Ok(Some(elected_gateway)) => Some(elected_gateway),
            _ => self
                .proposer_duties
                .read()
                .unwrap()
                .iter()
                .find(|duty| duty.slot == next_slot)
                .map(|duty| SignedPreconferElection::from_proposer_duty(duty, self.chain_info.context.deposit_chain_id as u64)),
        };

        let mut write_guard = self.curr_slot_info.write().unwrap();
        write_guard.slot = slot_update.slot;
        write_guard.next_elected_preconfer = elected_preconfer;
    }
}

fn get_nanos_timestamp() -> Result<u64, ConstraintsApiError> {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).map_err(|_| ConstraintsApiError::InternalServerError)
}

fn get_remaining_slots_for_epoch(slot: u64) -> Vec<u64> {
    let epoch = slot / SLOTS_PER_EPOCH;

    // get a list of slots for the epoch beginning at slot until the end of the epoch
    let all_slots: Vec<u64> = (epoch * SLOTS_PER_EPOCH..(epoch + 1) * SLOTS_PER_EPOCH).collect();
    let remaining_slots = all_slots.iter().filter(|&s| *s >= slot).map(|&s| s).collect();
    remaining_slots
}

fn get_remaining_slots_for_current_and_next_n_epochs(slot: u64, n: u64) -> Vec<u64> {
    let mut slots: Vec<u64> = get_remaining_slots_for_epoch(slot);

    // Calculate the starting epoch
    let start_epoch = slot / SLOTS_PER_EPOCH;

    // Add slots for the next n epochs
    for epoch in start_epoch + 1..=start_epoch + n {
        let epoch_slots: Vec<u64> = (epoch * SLOTS_PER_EPOCH..(epoch + 1) * SLOTS_PER_EPOCH).collect();
        slots.extend(epoch_slots);
    }

    slots
}

#[cfg(test)]
mod tests {
    use crate::constraints::api::get_remaining_slots_for_epoch;
    #[tokio::test]
    async fn test_get_remaining_slots_for_epoch() {
        let slots = get_remaining_slots_for_epoch(5);
        println!("{:?}", slots);
        assert_eq!(slots, vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]);
    }

    use crate::constraints::api::get_remaining_slots_for_current_and_next_n_epochs;
    #[tokio::test]
    async fn test_get_remaining_slots_for_epochs() {
        let slots = get_remaining_slots_for_current_and_next_n_epochs(5, 1);
        println!("{:?}", slots);
        assert_eq!(slots, vec![
            5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39,
            40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
        ]);
    }
}
