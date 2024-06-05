use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use ethereum_consensus::primitives::{BlsPublicKey, BlsSignature, Hash32};
use ethereum_consensus::types::mainnet::SignedBlindedBeaconBlock;
use tracing::{debug, warn};
use uuid::Uuid;

use helix_common::api::builder_api::BuilderGetValidatorsResponseEntry;
use helix_common::api::constraints_api::SignedGatewayElection;
use helix_common::bellatrix::SimpleSerialize;
use helix_common::chain_info::ChainInfo;
use helix_common::ProposerDuty;
use helix_common::traces::constraints_api::ElectGatewayTrace;
use helix_datastore::{Auctioneer, constraints::ConstraintsAuctioneer};
use helix_utils::signing::verify_signed_builder_message;

use crate::constraints::error::ConstraintsApiError;
use crate::proposer::api::MAX_BLINDED_BLOCK_LENGTH;
use crate::proposer::error::ProposerApiError;
use crate::proposer::GetHeaderParams;

pub(crate) const MAX_GATEWAY_ELECTION_SIZE: usize = 1024 * 1024; // TODO: this should be a fixed size that we calc

#[derive(Clone)]
pub struct ConstraintsApi<A>
where
    A: ConstraintsAuctioneer,
{
    auctioneer: A,

    chain_info: Arc<ChainInfo>,
    proposer_duties: Arc<RwLock<Vec<ProposerDuty>>>,

    /// Information about the current head slot and next proposer duty
    /// TODO: need to add preconf pubkey here as optional as this will overwrite who we expect preconf from
    curr_slot_info: Arc<RwLock<(u64, Option<BuilderGetValidatorsResponseEntry>)>>,
}

impl<A> ConstraintsApi<A>
where
    A: ConstraintsAuctioneer + 'static,
{
    /// Elects a gateway to perform pre-confirmations for a validator. The request must be signed by the validator
    /// and cannot be for a slot more than 2 epochs in the future.
    pub async fn elect_gateway(&self, req: Request<Body>) -> Result<impl IntoResponse, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = ElectGatewayTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        // Deserialise request
        let mut election_req: SignedGatewayElection = deserialize_json_request_bytes(req, MAX_GATEWAY_ELECTION_SIZE).await?;
        trace.deserialize = get_nanos_timestamp()?;

        let (head_slot, _) = *self.curr_slot_info.read().map_err(|_| ConstraintsApiError::LockPoisoned)?;
        debug!(
            request_id = %request_id,
            event = "elect_gateway",
            head_slot = head_slot,
            request_ts = trace.receive,
            slot = %election_req.slot(),
            parent_hash = ?election_req.parent_hash(),
            public_key = ?election_req.public_key(),
        );

        if let Err(err) = self.validate_election_request(&mut election_req, head_slot) {
            warn!(request_id = %request_id, ?err, "validation failed");
            return Err(err);
        }
        trace.validation_complete = get_nanos_timestamp()?;

        // Save to constraints datastore
        // TODO: database
        self.auctioneer.save_new_gateway_election(election_req.gateway_public_key(), election_req.slot()).await?;
        trace.gateway_election_saved = get_nanos_timestamp()?;

        Ok(StatusCode::OK)
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
            duty.slot == election_req.slot() && &duty.public_key == election_req.public_key()
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
}

async fn deserialize_json_request_bytes<T: serde::de::DeserializeOwned>(req: Request<Body>, max_size: usize) -> Result<T, ConstraintsApiError> {
    let body = req.into_body();
    let body_bytes = to_bytes(body, max_size).await?;
    Ok(serde_json::from_slice(&body_bytes)?)
}

fn get_nanos_timestamp() -> Result<u64, ConstraintsApiError> {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).map_err(|_| ConstraintsApiError::InternalServerError)
}
