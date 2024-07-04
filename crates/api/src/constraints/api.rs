use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ethereum_consensus::phase0::mainnet::SLOTS_PER_EPOCH;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, warn};
use uuid::Uuid;

use helix_common::api::builder_api::BuilderGetValidatorsResponseEntry;
use helix_common::api::constraints_api::{GetConstraintsParams, GetPreconferParams, SignedPreconferElection};
use helix_common::chain_info::ChainInfo;
use helix_common::traces::constraints_api::GetGatewayTrace;
use helix_datastore::constraints::ConstraintsAuctioneer;
use helix_housekeeper::{ChainUpdate, SlotUpdate};

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
    pub fn new(
        auctioneer: Arc<A>,
        chain_info: Arc<ChainInfo>,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
    ) -> Self {
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
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .body(axum::body::Body::from(constraints_bytes))
                    .unwrap()
                    .into_response()
                )
            }
            None => {
                Ok(StatusCode::NO_CONTENT.into_response())
            }
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
            event = "get_gateway",
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
                Err(ConstraintsApiError::NoPreconferFoundForSlot {slot})
            }
            Err(err) => {
                warn!(%request_id, ?err, "no gateway found for request");
                Err(ConstraintsApiError::NoPreconferFoundForSlot {slot})
            }
        }
    }
}

// STATE SYNC
impl<A> ConstraintsApi<A>
    where
        A: ConstraintsAuctioneer + 'static,
{
    /// Subscribes to slot head updater.
    /// Updates the current slot and next proposer duty.
    pub async fn housekeep(
        &self,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
    ) -> Result<(), SendError<Sender<ChainUpdate>>> {
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
            _ => {
                self.proposer_duties
                    .read()
                    .unwrap()
                    .iter()
                    .find(|duty| duty.slot == next_slot)
                    .map(|duty| SignedPreconferElection::from_proposer_duty(duty, self.chain_info.context.deposit_chain_id as u64))
            }
        };

        let mut write_guard = self.curr_slot_info.write().unwrap();
        write_guard.slot = slot_update.slot;
        write_guard.next_elected_preconfer = elected_preconfer;
    }
}

fn get_nanos_timestamp() -> Result<u64, ConstraintsApiError> {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).map_err(|_| ConstraintsApiError::InternalServerError)
}
