use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension,
};
use ethereum_consensus::{deneb::Slot, ssz};
use helix_common::{
    api::constraints_api::{
        SignableBLS, SignedDelegation, SignedRevocation, DELEGATION_ACTION,
        MAX_CONSTRAINTS_PER_SLOT, REVOCATION_ACTION,
    },
    bellatrix::List,
    chain_info::ChainInfo,
    proofs::{ConstraintsMessage, SignedConstraints, SignedConstraintsWithProofData},
    ConstraintSubmissionTrace,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_utils::signing::{verify_signed_message, COMMIT_BOOST_DOMAIN};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::broadcast, time::Instant};
use tracing::{error, info, trace, warn};
use uuid::Uuid;

use crate::constraints::error::ConstraintsApiError;

use super::error::Conflict;

// This is the maximum length (randomly chosen) of a request body in bytes.
pub(crate) const MAX_REQUEST_LENGTH: usize = 1024 * 1024 * 5;

#[derive(Clone)]
pub struct ConstraintsApi<A, DB>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
{
    auctioneer: Arc<A>,
    db: Arc<DB>,
    chain_info: Arc<ChainInfo>,

    constraints_handle: ConstraintsHandle,
}

#[derive(Clone)]
pub struct ConstraintsHandle {
    pub(crate) constraints_tx: broadcast::Sender<SignedConstraints>,
}

impl ConstraintsHandle {
    pub fn send_constraints(&self, constraints: SignedConstraints) {
        if self.constraints_tx.send(constraints).is_err() {
            error!("Failed to send constraints to the constraints channel");
        }
    }
}

impl<A, DB> ConstraintsApi<A, DB>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
{
    pub fn new(
        auctioneer: Arc<A>,
        db: Arc<DB>,
        chain_info: Arc<ChainInfo>,
        constraints_handle: ConstraintsHandle,
    ) -> Self {
        Self { auctioneer, db, chain_info, constraints_handle }
    }

    /// Handles the submission of batch of signed constraints.
    ///
    /// Implements this API: <https://docs.boltprotocol.xyz/api/builder#constraints>
    pub async fn submit_constraints(
        Extension(api): Extension<Arc<ConstraintsApi<A, DB>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace =
            ConstraintSubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        // Decode the incoming request body into a payload.
        let signed_constraints =
            decode_constraints_submission(req, &mut trace, &request_id).await?;

        if signed_constraints.is_empty() {
            return Err(ConstraintsApiError::NilConstraints)
        }

        // check that all constraints are for the same slot and with the same pubkey
        let Some(first_constraints) = signed_constraints.first().map(|c| c.message.clone()) else {
            error!(request_id = %request_id, "No constraints found");
            return Err(ConstraintsApiError::InvalidConstraints);
        };
        if !signed_constraints.iter().all(|c| c.message.slot == first_constraints.slot) {
            error!(request_id = %request_id, "Constraints for different slots in the same batch");
            return Err(ConstraintsApiError::InvalidConstraints)
        }
        if !signed_constraints.iter().all(|c| c.message.pubkey == first_constraints.pubkey) {
            error!(request_id = %request_id, "Constraints for different pubkeys in the same batch");
            return Err(ConstraintsApiError::InvalidConstraints)
        }

        // PERF: can we avoid calling the db?
        let maybe_validator_pubkey = api.db.get_proposer_duties().await?.iter().find_map(|d| {
            if d.slot == first_constraints.slot {
                Some(d.entry.registration.message.public_key.clone())
            } else {
                None
            }
        });

        let Some(validator_pubkey) = maybe_validator_pubkey else {
            error!(request_id = %request_id, slot = first_constraints.slot, "Missing proposer info");
            return Err(ConstraintsApiError::MissingProposerInfo);
        };

        // Fetch active delegations for the validator pubkey, if any
        let delegations =
            api.auctioneer.get_validator_delegations(validator_pubkey.clone()).await?;
        let delegatees =
            delegations.iter().map(|d| d.message.delegatee_pubkey.clone()).collect::<Vec<_>>();

        // Add all the valid constraints to the cache
        for constraint in signed_constraints {
            // Check for conflicts in the constraints
            let saved_constraints = api.auctioneer.get_constraints(constraint.message.slot).await?;
            if let Some(conflict) = conflicts_with(&saved_constraints, &constraint.message) {
                return Err(ConstraintsApiError::Conflict(conflict))
            }

            // Check if the maximum number of constraints per slot has been reached
            if saved_constraints.is_some_and(|c| c.len() + 1 > MAX_CONSTRAINTS_PER_SLOT) {
                return Err(ConstraintsApiError::MaxConstraintsReached)
            }

            // Check if the constraint pubkey is delegated to submit constraints for this validator.
            // - If there are no delegations, only the validator pubkey can submit constraints
            // - If there are delegations, only delegatees can submit constraints
            let message_not_signed_by_validator =
                delegatees.is_empty() && constraint.message.pubkey != validator_pubkey;
            let message_not_signed_by_delegatee =
                !delegatees.is_empty() && !delegatees.contains(&constraint.message.pubkey);

            if message_not_signed_by_validator && message_not_signed_by_delegatee {
                error!(request_id = %request_id, pubkey = %constraint.message.pubkey, "Pubkey unauthorized");
                return Err(ConstraintsApiError::PubkeyNotAuthorized(constraint.message.pubkey))
            }

            // Verify the constraints message BLS signature
            if let Err(e) = verify_signed_message(
                &constraint.message.digest(),
                &constraint.signature,
                &constraint.message.pubkey,
                COMMIT_BOOST_DOMAIN,
                &api.chain_info.context,
            ) {
                error!(err = ?e, request_id = %request_id, "Invalid constraints signature");
                return Err(ConstraintsApiError::InvalidSignature)
            };

            // Send to the constraints channel
            api.constraints_handle.send_constraints(constraint.clone());

            // Finally add the constraints to the redis cache
            if let Err(err) = api
                .save_constraints_to_auctioneer(
                    &mut trace,
                    constraint.message.slot,
                    constraint,
                    &request_id,
                )
                .await
            {
                error!(request_id = %request_id, error = %err, "Failed to save constraints to auctioneer");
            };
        }

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        trace!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "submit_constraints request finished",
        );

        Ok(StatusCode::OK)
    }

    /// Handles delegating constraint submission rights to another BLS key.
    ///
    /// Implements this API: <https://docs.boltprotocol.xyz/api/builder#delegate>
    pub async fn delegate(
        Extension(api): Extension<Arc<ConstraintsApi<A, DB>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace =
            ConstraintSubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        info!(
            request_id = %request_id,
            event = "delegate",
            timestamp_request_start = trace.receive,
        );

        // Read the body
        let body = req.into_body();
        let body_bytes = to_bytes(body, MAX_REQUEST_LENGTH).await?;

        // Decode the incoming request body into a `SignedDelegation` array.
        let signed_delegations = match serde_json::from_slice::<Vec<SignedDelegation>>(&body_bytes)
        {
            Ok(delegations) => {
                let action = delegations.iter().map(|d| d.message.action).collect::<HashSet<_>>();
                let are_all_actions_delegations =
                    action.len() == 1 && action.contains(&DELEGATION_ACTION);
                if !are_all_actions_delegations {
                    warn!(request_id = %request_id, actions = ?action, "Invalid delegations action. expected {DELEGATION_ACTION}");
                    return Err(ConstraintsApiError::InvalidDelegation)
                }
                delegations
            }
            Err(e) => {
                warn!(err = ?e, request_id = %request_id, "Failed to decode delegations");
                return Err(ConstraintsApiError::InvalidDelegation)
            }
        };
        trace.decode = get_nanos_timestamp()?;

        for delegation in &signed_delegations {
            if let Err(e) = verify_signed_message(
                &delegation.message.digest(),
                &delegation.signature,
                &delegation.message.validator_pubkey,
                COMMIT_BOOST_DOMAIN,
                &api.chain_info.context,
            ) {
                warn!(err = ?e, request_id = %request_id, "Invalid delegation signature");
                return Err(ConstraintsApiError::InvalidSignature)
            };
        }
        trace.verify_signature = get_nanos_timestamp()?;

        // Store the delegation in the database
        tokio::spawn(async move {
            if let Err(err) = api.auctioneer.save_validator_delegations(signed_delegations).await {
                error!(error = %err, "Failed to save delegations");
            }
        });

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        trace!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "delegate request finished",
        );

        Ok(StatusCode::OK)
    }

    /// Handles revoking constraint submission rights from a BLS key.
    ///
    /// Implements this API: <https://docs.boltprotocol.xyz/api/builder#revoke>
    pub async fn revoke(
        Extension(api): Extension<Arc<ConstraintsApi<A, DB>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace =
            ConstraintSubmissionTrace { receive: get_nanos_timestamp()?, ..Default::default() };

        info!(
            request_id = %request_id,
            event = "revoke",
            timestamp_request_start = trace.receive,
        );

        // Read the body
        let body = req.into_body();
        let body_bytes = to_bytes(body, MAX_REQUEST_LENGTH).await?;

        // Decode the incoming request body into a `SignedRevocation` array.
        let signed_revocations = match serde_json::from_slice::<Vec<SignedRevocation>>(&body_bytes)
        {
            Ok(revocations) => {
                let action = revocations.iter().map(|r| r.message.action).collect::<HashSet<_>>();
                let are_all_actions_revocations =
                    action.len() == 1 && action.contains(&REVOCATION_ACTION);
                if !are_all_actions_revocations {
                    warn!(request_id = %request_id, actions = ?action, "Invalid revocation action. expected {REVOCATION_ACTION}");
                    return Err(ConstraintsApiError::InvalidRevocation)
                }
                revocations
            }
            Err(e) => {
                warn!(err = ?e, request_id = %request_id, "Failed to decode revocation");
                return Err(ConstraintsApiError::InvalidRevocation)
            }
        };
        trace.decode = get_nanos_timestamp()?;

        for revocation in &signed_revocations {
            if let Err(e) = verify_signed_message(
                &revocation.message.digest(),
                &revocation.signature,
                &revocation.message.validator_pubkey,
                COMMIT_BOOST_DOMAIN,
                &api.chain_info.context,
            ) {
                warn!(err = ?e, request_id = %request_id, "Invalid revocation signature");
                return Err(ConstraintsApiError::InvalidSignature)
            };
        }
        trace.verify_signature = get_nanos_timestamp()?;

        // Store the delegation in the database
        tokio::spawn(async move {
            if let Err(err) = api.auctioneer.revoke_validator_delegations(signed_revocations).await
            {
                error!(error = %err, "Failed to do revocation");
            }
        });

        // Log some final info
        trace.request_finish = get_nanos_timestamp()?;
        info!(
            request_id = %request_id,
            trace = ?trace,
            request_duration_ns = trace.request_finish.saturating_sub(trace.receive),
            "revoke request finished",
        );

        Ok(StatusCode::OK)
    }
}

// Helpers
impl<A, DB> ConstraintsApi<A, DB>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
{
    async fn save_constraints_to_auctioneer(
        &self,
        trace: &mut ConstraintSubmissionTrace,
        slot: Slot,
        constraint: SignedConstraints,
        request_id: &Uuid,
    ) -> Result<(), ConstraintsApiError> {
        let message_with_data = SignedConstraintsWithProofData::try_from(constraint)?;
        match self.auctioneer.save_constraints(slot, message_with_data).await {
            Ok(()) => {
                trace.auctioneer_update = get_nanos_timestamp()?;
                info!(
                    request_id = %request_id,
                    timestamp_after_auctioneer = Instant::now().elapsed().as_nanos(),
                    auctioneer_latency_ns = trace.auctioneer_update.saturating_sub(trace.decode),
                    "Constraints saved to auctioneer",
                );
                Ok(())
            }
            Err(err) => {
                error!(request_id = %request_id, error = %err, "Failed to save constraints to auctioneer");
                Err(ConstraintsApiError::AuctioneerError(err))
            }
        }
    }
}

/// Checks if the constraints for the given slot conflict with the existing constraints.
/// Returns a [Conflict] in case of a conflict, None otherwise.
///
/// # Possible conflicts
/// - Multiple ToB constraints per slot
/// - Duplicates of the same transaction per slot
pub fn conflicts_with(
    saved_constraints: &Option<Vec<SignedConstraintsWithProofData>>,
    constraints: &ConstraintsMessage,
) -> Option<Conflict> {
    // Check if there are saved constraints to compare against
    if let Some(saved_constraints) = saved_constraints {
        for saved_constraint in saved_constraints {
            // Only 1 ToB (Top of Block) constraint per slot
            if constraints.top && saved_constraint.signed_constraints.message.top {
                return Some(Conflict::TopOfBlock)
            }

            // Check if any of the transactions are the same
            for tx in constraints.transactions.iter() {
                if saved_constraint
                    .signed_constraints
                    .message
                    .transactions
                    .iter()
                    .any(|existing| tx == existing)
                {
                    return Some(Conflict::DuplicateTransaction)
                }
            }
        }
    }

    None
}

pub async fn decode_constraints_submission(
    req: Request<Body>,
    trace: &mut ConstraintSubmissionTrace,
    request_id: &Uuid,
) -> Result<Vec<SignedConstraints>, ConstraintsApiError> {
    // Check if the request is SSZ encoded
    let is_ssz = req
        .headers()
        .get("Content-Type")
        .and_then(|val| val.to_str().ok())
        .map_or(false, |v| v == "application/octet-stream");

    // Read the body
    let body = req.into_body();
    let body_bytes = to_bytes(body, MAX_REQUEST_LENGTH).await?;

    // Decode the body
    let constraints: List<SignedConstraints, MAX_CONSTRAINTS_PER_SLOT> = if is_ssz {
        match ssz::prelude::deserialize(&body_bytes) {
            Ok(constraints) => constraints,
            Err(err) => {
                // Fallback for JSON
                warn!(request_id = %request_id, error = %err, "Failed to decode SSZ constraints, falling back to JSON");
                serde_json::from_slice(&body_bytes)?
            }
        }
    } else {
        serde_json::from_slice(&body_bytes)?
    };

    trace.decode = get_nanos_timestamp()?;
    info!(
        request_id = %request_id,
        timestamp_after_decoding = Instant::now().elapsed().as_nanos(),
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        num_constraints = constraints.len(),
    );

    Ok(constraints.to_vec())
}

fn get_nanos_timestamp() -> Result<u64, ConstraintsApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .map_err(|_| ConstraintsApiError::InternalError)
}
