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
use helix_common::api::constraints_api::{ConstraintsMessage, GetGatewayParams, SignedConstraintsMessage, SignedGatewayElection};
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
#[derive(Clone)]
struct SlotInfo {
    pub slot: u64,
    pub elected_gateway: Option<BlsPublicKey>,
}

#[derive(Clone)]
pub struct ConstraintsApi<A>
where
    A: ConstraintsAuctioneer,
{
    auctioneer: A,

    chain_info: Arc<ChainInfo>,
    proposer_duties: Arc<RwLock<Vec<BuilderGetValidatorsResponseEntry>>>,
    curr_slot_info: Arc<RwLock<SlotInfo>>,
}

impl<A> ConstraintsApi<A>
where
    A: ConstraintsAuctioneer + 'static,
{
    /// Returns the constraints for the current slot
    pub async fn get_constraints(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
    ) -> Result<impl IntoResponse, ConstraintsApiError> {
        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;

        let constraints = api.auctioneer.get_constraints(head_slot).await?;
        let constraints_bytes = serde_json::to_vec(&constraints)?;

        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(axum::body::Body::from(constraints_bytes))
            .unwrap()
            .into_response()
        )
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
        let mut election_req: SignedGatewayElection = deserialize_json_request_bytes(req, MAX_GATEWAY_ELECTION_SIZE).await?;
        trace.deserialize = get_nanos_timestamp()?;

        let head_slot = api.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?.slot;
        debug!(
            request_id = %request_id,
            event = "elect_gateway",
            head_slot = head_slot,
            request_ts = trace.receive,
            slot = %election_req.slot(),
            public_key = ?election_req.public_key(),
            validator_index=%election_req.validator_index(),
        );

        if let Err(err) = api.validate_election_request(&mut election_req, head_slot) {
            warn!(request_id = %request_id, ?err, "validation failed");
            return Err(err);
        }
        trace.validation_complete = get_nanos_timestamp()?;

        // Save to constraints datastore
        // TODO: database
        api.auctioneer.save_new_gateway_election(election_req.gateway_public_key(), election_req.slot()).await?;
        trace.gateway_election_saved = get_nanos_timestamp()?;

        info!(%request_id, ?trace, "gateway elected");
        Ok(StatusCode::OK)
    }

    /// Returns the gateway for the given slot. If the request is for a proposer in the next 2 epochs, it will always
    /// return something. If no elected gateway is found, it defaults to the proposer public key.
    pub async fn get_gateway(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        Path(GetGatewayParams { slot }): Path<GetGatewayParams>,
    ) -> Result<BlsPublicKey, ConstraintsApiError> {
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

        match api.get_elected_gateway_for_slot(slot).await {
            Ok(elected_gateway) => {
                trace.gateway_fetched = get_nanos_timestamp()?;
                debug!(%request_id, ?elected_gateway, ?trace, "found elected gateway");
                return Ok(elected_gateway);
            }
            Err(err) => {
                warn!(%request_id, ?err, "no gateway found for request");
                Err(ConstraintsApiError::NoGatewayFoundForSlot {slot})
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
        debug!(
            request_id = %request_id,
            event = "set_constraints",
            head_slot = slot_info.slot,
            request_ts = trace.receive,
            request_slot = %constraints.slot(),
            num_constraints = %constraints.constraints().len(),
        );

        if slot_info.elected_gateway.is_none() {
            warn!(request_id = %request_id, "no elected gateway set for slot");
            return Err(ConstraintsApiError::NoGatewayFoundForSlot { slot: slot_info.slot });
        }

        // Validate request
        if let Err(err) = api.validate_set_constraints_request(
            &mut constraints,
            slot_info.elected_gateway.as_ref().unwrap(),
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
        elected_gateway: &BlsPublicKey,
        head_slot: u64,
        receive_ns: u64,
    ) -> Result<(), ConstraintsApiError> {
        // Can only set constraints for the current slot.
        if constraints.slot() != head_slot {
            return Err(ConstraintsApiError::CanOnlySetConstraintsForCurrentSlot { request_slot: constraints.slot(), curr_slot: head_slot });
        }

        // Constraints cannot be set more than `SET_CONSTRAINTS_CUTOFF_NS` into the previous slot.
        let slot_start_timestamp = self.chain_info.genesis_time_in_secs +
            (head_slot * self.chain_info.seconds_per_slot);
        let ns_into_slot = (receive_ns as i64).saturating_sub((slot_start_timestamp * 1_000_000_000) as i64);
        if ns_into_slot > SET_CONSTRAINTS_CUTOFF_NS {
            return Err(ConstraintsApiError::SetConstraintsTooLate {
                ns_into_slot: ns_into_slot as u64,
                cutoff: GET_HEADER_REQUEST_CUTOFF_MS as u64,
            });
        }

        // Ensure the constraint request is from the expected public key
        if constraints.public_key() != elected_gateway {
            return Err(ConstraintsApiError::NotElectedGateway {
                request_public_key: constraints.public_key().clone(),
                elected_gateway_public_key: elected_gateway.clone(),
            });
        }

        // Verify proposer signature
        if let Err(err) = verify_signed_builder_message(
            &mut constraints.message,
            &constraints.signature,
            elected_gateway,
            &self.chain_info.context,
        ) {
            return Err(ConstraintsApiError::InvalidSignature(err));
        }

        // Check we haven't already received constraints for this slot
        if self.auctioneer.get_constraints(head_slot).await?.is_some() {
            return Err(ConstraintsApiError::ConstraintsAlreadySetForSlot);
        }

        Ok(())
    }

    /// - Checks if the requested slot is in the past.
    /// - Retrieves the latest known proposer duty.
    /// - Ensures the request slot is not beyond the latest known proposer duty.
    /// - Validates that the provided public key is the proposer for the requested slot.
    /// - Verifies the signature.
    fn validate_election_request(&self, election_req: &mut SignedGatewayElection, head_slot: u64) -> Result<(), ConstraintsApiError> {
        // Cannot elect a gateway for a past slot
        if election_req.slot() < head_slot {
            return Err(ConstraintsApiError::RequestForPastSlot { request_slot: election_req.slot(), head_slot });
        }

        let duties_read_guard = self.proposer_duties.read().map_err(|_| ConstraintsApiError::LockPoisoned)?;

        // Determine max known proposer duty and ensure the request isn't for a slot beyond that
        let latest_known_proposer_duty = duties_read_guard.last().ok_or(ConstraintsApiError::ProposerDutiesNotKnown)?;
        if election_req.slot() > latest_known_proposer_duty.slot {
            return Err(ConstraintsApiError::CannotElectGatewayTooFarInTheFuture {
                request_slot: election_req.slot(),
                max_slot: latest_known_proposer_duty.slot,
            });
        }

        // Ensure provided validator public key is the proposer for the requested slot.
        if !duties_read_guard.iter().any(|duty|
            duty.slot == election_req.slot() &&
                &duty.entry.registration.message.public_key == election_req.public_key() &&
                duty.validator_index == election_req.validator_index()
        ) {
            return Err(ConstraintsApiError::ValidatorIsNotProposerForRequestedSlot);
        }

        // Drop the read lock guard to avoid holding it during signature verification
        drop(duties_read_guard);

        // Verify proposer signature
        let req_proposer_public_key = election_req.public_key().clone();
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

    async fn get_elected_gateway_for_slot(&self, slot: u64) -> Result<BlsPublicKey, ConstraintsApiError> {
        // Try to fetch from datastore
        if let Some(elected_gateway) = self.auctioneer.get_gateway(slot).await? {
            return Ok(elected_gateway);
        }

        // If it can't be found in the datastore then we default to checking the proposer duties
        let duties_read_guard = self.proposer_duties.read().map_err(|_| ConstraintsApiError::LockPoisoned)?;
        match duties_read_guard.iter().find(|duty| duty.slot == slot) {
            Some(proposer_duty) => {
                Ok(proposer_duty.entry.registration.message.public_key.clone())
            }
            None => {
                Err(ConstraintsApiError::NoGatewayFoundForSlot {slot})
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

        // Update duties if applicable
        if let Some(new_duties) = slot_update.new_duties {
            *self.proposer_duties.write().unwrap() = new_duties;
        }

        // Fetch elected gateway for the current slot
        match self.get_elected_gateway_for_slot(slot_update.slot).await {
            Ok(elected_gateway) => {
                let mut write_guard = self.curr_slot_info.write().unwrap();
                write_guard.slot = slot_update.slot;
                write_guard.elected_gateway = Some(elected_gateway);
            }
            Err(err) => {
                error!(?err, "could not fetch elected gateway for head slot");
                let mut write_guard = self.curr_slot_info.write().unwrap();
                write_guard.slot = slot_update.slot;
                write_guard.elected_gateway = None;
            }
        }
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
