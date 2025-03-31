use std::{self, collections::HashSet, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension,
};
use ethereum_consensus::{phase0::mainnet::SLOTS_PER_EPOCH, ssz};
use helix_common::{
    api::constraints_api::{
        SignableBLS, SignedDelegation, SignedRevocation, DELEGATION_ACTION,
        MAX_CONSTRAINTS_PER_SLOT, REVOCATION_ACTION,
    },
    bellatrix::List,
    chain_info::ChainInfo,
    proofs::{ConstraintsMessage, SignedConstraints, SignedConstraintsWithProofData},
    task, ConstraintSubmissionTrace, ConstraintsApiConfig,
};
use helix_datastore::Auctioneer;
use helix_housekeeper::{ChainUpdate, SlotUpdate};
use helix_utils::{
    signing::{verify_signed_message, COMMIT_BOOST_DOMAIN},
    utcnow_ns,
};
use tokio::{
    sync::{
        broadcast,
        mpsc::{self, error::SendError, Sender},
        RwLock,
    },
    time::Instant,
};
use tracing::{error, info, trace, warn};
use uuid::Uuid;

use super::error::Conflict;
use crate::constraints::error::ConstraintsApiError;

// This is the maximum length (randomly chosen) of a request body in bytes.
pub(crate) const MAX_REQUEST_LENGTH: usize = 1024 * 1024 * 5;

#[derive(Clone)]
pub struct ConstraintsApi<A>
where
    A: Auctioneer + 'static,
{
    auctioneer: Arc<A>,
    chain_info: Arc<ChainInfo>,
    /// Information about the current head slot and next proposer duty
    curr_slot_info: Arc<RwLock<SlotUpdate>>,

    constraints_api_config: Arc<ConstraintsApiConfig>,

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

impl<A> ConstraintsApi<A>
where
    A: Auctioneer + 'static,
{
    pub fn new(
        auctioneer: Arc<A>,
        chain_info: Arc<ChainInfo>,
        slot_update_subscription: Sender<Sender<ChainUpdate>>,
        constraints_handle: ConstraintsHandle,
        constraints_api_config: Arc<ConstraintsApiConfig>,
    ) -> Self {
        let api = Self {
            auctioneer,
            chain_info,
            curr_slot_info: Arc::new(RwLock::new(Default::default())),
            constraints_handle,
            constraints_api_config,
        };

        // Spin up the housekeep task
        let api_clone = api.clone();
        task::spawn(file!(), line!(), async move {
            if let Err(err) = api_clone.housekeep(slot_update_subscription).await {
                error!(
                    error = %err,
                    "ConstraintsApi. housekeep task encountered an error",
                );
            }
        });

        api
    }

    /// Handles the submission of batch of signed constraints.
    ///
    /// Implements this API: <https://docs.boltprotocol.xyz/technical-docs/api/builder#constraints>
    pub async fn submit_constraints(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = ConstraintSubmissionTrace { receive: utcnow_ns(), ..Default::default() };

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

        let maybe_validator_pubkey =
            if let Some(duties) = api.curr_slot_info.read().await.new_duties.as_ref() {
                duties.iter().find_map(|d| {
                    if d.slot == first_constraints.slot {
                        Some(d.entry.registration.message.public_key.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            };

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

            if api.constraints_api_config.check_constraints_signature {
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
            }

            // Send to the constraints channel
            api.constraints_handle.send_constraints(constraint.clone());

            // Decode the constraints and generate proof data.
            let constraints_with_proofs = SignedConstraintsWithProofData::try_from(constraint).inspect_err(|err| {
                error!(%err, %request_id, "Failed to decode constraints transactions and generate proof data");
            })?;

            // Finally add the constraints to the redis cache
            api.save_constraints_to_auctioneer(&mut trace, constraints_with_proofs, &request_id)
                .await.map_err(|err| {
                    error!(request_id = %request_id, error = %err, "Failed to save constraints to auctioneer");
                    err
                })?;
        }

        // Log some final info
        trace.request_finish = utcnow_ns();
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
    /// Implements this API: <https://docs.boltprotocol.xyz/technical-docs/api/builder#delegate>
    pub async fn delegate(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = ConstraintSubmissionTrace { receive: utcnow_ns(), ..Default::default() };

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
        trace.decode = utcnow_ns();

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
        trace.verify_signature = utcnow_ns();

        // Store the delegation in the database
        task::spawn(file!(), line!(), async move {
            if let Err(err) = api.auctioneer.save_validator_delegations(signed_delegations).await {
                error!(error = %err, "Failed to save delegations");
            }
        });

        // Log some final info
        trace.request_finish = utcnow_ns();
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
    /// Implements this API: <https://docs.boltprotocol.xyz/technical-docs/api/builder#revoke>
    pub async fn revoke(
        Extension(api): Extension<Arc<ConstraintsApi<A>>>,
        req: Request<Body>,
    ) -> Result<StatusCode, ConstraintsApiError> {
        let request_id = Uuid::new_v4();
        let mut trace = ConstraintSubmissionTrace { receive: utcnow_ns(), ..Default::default() };

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
        trace.decode = utcnow_ns();

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
        trace.verify_signature = utcnow_ns();

        // Store the delegation in the database
        task::spawn(file!(), line!(), async move {
            if let Err(err) = api.auctioneer.revoke_validator_delegations(signed_revocations).await
            {
                error!(error = %err, "Failed to do revocation");
            }
        });

        // Log some final info
        trace.request_finish = utcnow_ns();
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
impl<A> ConstraintsApi<A>
where
    A: Auctioneer + 'static,
{
    async fn save_constraints_to_auctioneer(
        &self,
        trace: &mut ConstraintSubmissionTrace,
        constraints_with_proofs: SignedConstraintsWithProofData,
        request_id: &Uuid,
    ) -> Result<(), ConstraintsApiError> {
        match self
            .auctioneer
            .save_constraints(
                constraints_with_proofs.signed_constraints.message.slot,
                constraints_with_proofs,
            )
            .await
        {
            Ok(()) => {
                trace.auctioneer_update = utcnow_ns();
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

// STATE SYNC
impl<A> ConstraintsApi<A>
where
    A: Auctioneer + 'static,
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
            if let ChainUpdate::SlotUpdate(slot_update) = slot_update {
                self.handle_new_slot(slot_update).await;
            }
        }

        Ok(())
    }

    /// Handle a new slot update.
    /// Updates the next proposer duty for the new slot.
    async fn handle_new_slot(&self, slot_update: SlotUpdate) {
        let epoch = slot_update.slot / self.chain_info.seconds_per_slot;
        info!(
            epoch = epoch,
            slot = slot_update.slot,
            slot_start_next_epoch = (epoch + 1) * SLOTS_PER_EPOCH,
            next_proposer_duty = ?slot_update.next_duty,
            "ConstraintsApi - housekeep: Updated head slot",
        );

        *self.curr_slot_info.write().await = slot_update
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

    trace.decode = utcnow_ns();
    info!(
        request_id = %request_id,
        timestamp_after_decoding = Instant::now().elapsed().as_nanos(),
        decode_latency_ns = trace.decode.saturating_sub(trace.receive),
        num_constraints = constraints.len(),
    );

    Ok(constraints.to_vec())
}
