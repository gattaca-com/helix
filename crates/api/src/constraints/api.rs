use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::Extension;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use ethereum_consensus::crypto::PublicKey;
use ethereum_consensus::phase0::mainnet::SLOTS_PER_EPOCH;
use ethereum_consensus::primitives::{BlsPublicKey, BlsSignature, Hash32};
use ethereum_consensus::types::mainnet::SignedBlindedBeaconBlock;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use helix_beacon_client::MultiBeaconClientTrait;

use helix_common::api::builder_api::BuilderGetValidatorsResponseEntry;
use helix_common::api::constraints_api::{ConstraintsMessage, ElectedPreconfer, GatewayInfo, GetGatewayParams, SignedConstraintsMessage, SignedPreconferElection};
use helix_common::bellatrix::SimpleSerialize;
use helix_common::builder_api::BuilderGetValidatorsResponse;
use helix_common::chain_info::ChainInfo;
use helix_common::ProposerDuty;
use helix_common::traces::constraints_api::{ElectGatewayTrace, GetGatewayTrace, SetConstraintsTrace};
use helix_database::DatabaseService;
use helix_datastore::{Auctioneer, constraints::ConstraintsAuctioneer};
use helix_housekeeper::{ChainUpdate, PayloadAttributesUpdate, SlotUpdate};
use helix_utils::get_payload_attributes_key;
use helix_utils::signing::verify_signed_builder_message;
use crate::builder::api::BuilderApi;

use crate::constraints::error::ConstraintsApiError;
use crate::constraints::SET_CONSTRAINTS_CUTOFF_NS;
use crate::proposer::api::{MAX_BLINDED_BLOCK_LENGTH, ProposerApi};
use crate::proposer::error::ProposerApiError;
use crate::proposer::{GET_HEADER_REQUEST_CUTOFF_MS, GetHeaderParams};

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
    ) -> Result<impl IntoResponse, ConstraintsApiError> {
        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;

        match api.auctioneer.get_constraints(head_slot).await? {
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

    /// Elects a gateway to perform pre-confirmations for a validator. The request must be signed by the validator
    /// and cannot be for a slot more than 2 epochs in the future.
    pub async fn elect_gateway(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = ElectGatewayTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        // Deserialise request
        let mut election_req: SignedPreconferElection = deserialize_json_request_bytes(req, MAX_GATEWAY_ELECTION_SIZE).await?;
        trace.deserialize = get_nanos_timestamp()?;

        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;
        info!(
            request_id = %request_id,
            event = "elect_gateway",
            head_slot = head_slot,
            request_ts = trace.receive,
            slot = %election_req.slot(),
            public_key = ?election_req.proposer_public_key(),
            validator_index=%election_req.validator_index(),
        );

        if let Err(err) = api.validate_election_request(&mut election_req, head_slot) {
            warn!(request_id = %request_id, ?err, "validation failed");
            return Err(err);
        }
        trace.validation_complete = get_nanos_timestamp()?;

        // Save to constraints datastore
        // TODO: database
        api.auctioneer.save_new_gateway_election(&election_req, election_req.slot()).await?;
        trace.gateway_election_saved = get_nanos_timestamp()?;

        info!(%request_id, ?trace, "gateway elected");
        Ok(StatusCode::OK)
    }

    /// Returns the gateway for the given slot. If no elected gateway is found, it returns an error.
    pub async fn get_gateway(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        Path(GetGatewayParams { slot }): Path<GetGatewayParams>,
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

        if slot < head_slot {
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

    /// If the request is sent by the preconf for this current slot and this is the first time. We save the constraints.
    /// must also be sent before the cutoff. TODO: fix comment
    pub async fn set_constraints(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = SetConstraintsTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        // Deserialise request
        let mut constraints: SignedConstraintsMessage = deserialize_json_request_bytes(req, MAX_SET_CONSTRAINTS_SIZE).await?;
        trace.deserialize = get_nanos_timestamp()?;

        let slot_info = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.clone();
        info!(
            request_id = %request_id,
            event = "set_constraints",
            head_slot = slot_info.slot,
            request_ts = trace.receive,
            request_slot = %constraints.slot(),
            num_constraints = %constraints.constraints().len(),
        );

        if slot_info.next_elected_preconfer.is_none() {
            warn!(request_id = %request_id, "no elected preconfer set for slot");
            return Err(ConstraintsApiError::NoPreconferFoundForSlot { slot: constraints.slot() });
        }

        // Validate request
        if let Err(err) = api.validate_set_constraints_request(
            &mut constraints,
            &slot_info.next_elected_preconfer.as_ref().unwrap().message,
            slot_info.slot,
            trace.receive,
        ).await {
            warn!(request_id = %request_id, ?err, "validation failed");
            return Err(err);
        }
        trace.validation_complete = get_nanos_timestamp()?;

        api.auctioneer.save_constraints(&constraints.message).await?;
        trace.constraints_set = get_nanos_timestamp()?;

        info!(%request_id, ?trace, "constraints set");
        Ok(StatusCode::OK)
    }

    /// - Ensures the constraints can only be set for the current slot.
    /// - Checks that the constraints are set within the allowed time window.
    /// - Verifies that the constraint request is from the expected public key.
    /// - Verifies the signature of the request matches the elected gateway.
    /// - Checks if we have already received constraints for the current slot.
    async fn validate_set_constraints_request(
        &self,
        constraints: &mut SignedConstraintsMessage,
        elected_preconfer: &ElectedPreconfer,
        head_slot: u64,
        receive_ns: u64,
    ) -> Result<(), ConstraintsApiError> {
        // Can only set constraints for the current slot.
        if constraints.slot() <= head_slot {
            return Err(ConstraintsApiError::CanOnlySetConstraintsForCurrentSlot { request_slot: constraints.slot(), curr_slot: head_slot + 1 });
        }

        // Constraints cannot be set more than `SET_CONSTRAINTS_CUTOFF_NS` into the previous slot.
        let slot_start_timestamp = self.chain_info.genesis_time_in_secs +
            (head_slot * self.chain_info.seconds_per_slot);
        let ns_into_slot = (receive_ns as i64).saturating_sub((slot_start_timestamp * 1_000_000_000) as i64);
        if ns_into_slot > SET_CONSTRAINTS_CUTOFF_NS {
            return Err(ConstraintsApiError::SetConstraintsTooLate {
                ns_into_slot: ns_into_slot as u64,
                cutoff: SET_CONSTRAINTS_CUTOFF_NS as u64,
            });
        }

        let elected_public_key = if elected_preconfer.gateway_info == GatewayInfo::default() {
            // Proposer is doing preconf commitments (e.g., Bolt proposer)
            &elected_preconfer.proposer_public_key
        } else {
            &elected_preconfer.gateway_info.gateway_public_key
        };

        // TODO: add back when we figure out Optional values for sigp TreeHash
        // let elected_public_key = match &elected_preconfer.gateway_info {
        //     Some(gateway_info) => {
        //         &gateway_info.gateway_public_key
        //     }
        //     None => {
        //         // Proposer is doing preconf commitments (e.g., Bolt proposer)
        //         &elected_preconfer.proposer_public_key
        //     }
        // };

        // If gateway public key is set then verify the elected key matches.
        if constraints.message.sent_by_gateway() {
            if constraints.gateway_public_key() != elected_public_key {
                return Err(ConstraintsApiError::NotPreconfer {
                    request_public_key: constraints.gateway_public_key().clone(),
                    preconfer_public_key: elected_public_key.clone(),
                });
            }
        }
        // if let Some(constraints_gateway_key) = constraints.gateway_public_key() {
        //     if constraints_gateway_key != elected_public_key {
        //         return Err(ConstraintsApiError::NotPreconfer {
        //             request_public_key: constraints_gateway_key.clone(),
        //             preconfer_public_key: elected_public_key.clone(),
        //         });
        //     }
        // }

        // Verify proposer signature
        if let Err(err) = verify_signed_builder_message(
            &mut constraints.message,
            &constraints.signature,
            elected_public_key,
            &self.chain_info.context,
        ) {
            return Err(ConstraintsApiError::InvalidSignature(err));
        }

        Ok(())
    }

    /// - Checks if the requested slot is in the past.
    /// - Retrieves the latest known proposer duty.
    /// - Ensures the request slot is not beyond the latest known proposer duty.
    /// - Validates that the provided public key is the proposer for the requested slot.
    /// - Verifies the signature.
    fn validate_election_request(&self, election_req: &mut SignedPreconferElection, head_slot: u64) -> Result<(), ConstraintsApiError> {
        // Cannot elect a gateway for a past slot
        if election_req.slot() < head_slot {
            return Err(ConstraintsApiError::RequestForPastSlot { request_slot: election_req.slot(), head_slot });
        }

        let duties_read_guard = self.proposer_duties.read().map_err(|_| ConstraintsApiError::LockPoisoned)?;

        // Ensure provided validator public key is the proposer for the requested slot.
        match duties_read_guard.iter().find(|duty| duty.slot == election_req.slot()) {
            Some(slot_duty) => {
                if !(&slot_duty.entry.registration.message.public_key == election_req.proposer_public_key() &&
                    slot_duty.validator_index == election_req.validator_index())
                {
                    return Err(ConstraintsApiError::ValidatorIsNotProposerForRequestedSlot);
                }
            }
            None => {
                return Err(ConstraintsApiError::ProposerDutyNotFound {
                    slot: election_req.slot(),
                    validator_index: election_req.validator_index(),
                })
            }
        }

        if !duties_read_guard.iter().any(|duty|
            duty.slot == election_req.slot() &&
                &duty.entry.registration.message.public_key == election_req.proposer_public_key() &&
                duty.validator_index == election_req.validator_index()
        ) {
            return Err(ConstraintsApiError::ValidatorIsNotProposerForRequestedSlot);
        }

        // Drop the read lock guard to avoid holding it during signature verification
        drop(duties_read_guard);

        // Verify proposer signature
        let req_proposer_public_key = election_req.proposer_public_key().clone();
        if let Err(err) = verify_signed_builder_message(
            &mut election_req.message,
            &election_req.signature,
            &req_proposer_public_key,
            &self.chain_info.context,
        ) {
            return Err(ConstraintsApiError::InvalidSignature(err));
        }

        Ok(())
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
                    .map(|duty| SignedPreconferElection::from_proposer_duty(duty))
            }
        };

        let mut write_guard = self.curr_slot_info.write().unwrap();
        write_guard.slot = slot_update.slot;
        write_guard.next_elected_preconfer = elected_preconfer;
    }
}

async fn deserialize_json_request_bytes<T: serde::de::DeserializeOwned>(req: Request<Body>, max_size: usize) -> Result<T, ConstraintsApiError> {
    let body = req.into_body();
    let body_bytes = to_bytes(body, max_size).await?;
    Ok(serde_json::from_slice(&body_bytes)?)
}

fn get_nanos_timestamp() -> Result<u64, ConstraintsApiError> {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).map_err(|_| ConstraintsApiError::InternalServerError)
}
