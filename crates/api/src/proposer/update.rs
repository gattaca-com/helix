use std::{collections::HashSet, sync::Arc};

use axum::{
    extract::{Json, Path},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Extension,
};
use helix_common::{utils::extract_request_id, ValidatorPreferences};
use helix_database::DatabaseService;
use helix_types::{BlsPublicKey, BlsPublicKeyBytes};
use tokio::time::Instant;
use tracing::{debug, warn};

use super::ProposerApi;
use crate::{
    proposer::{
        error::ProposerApiError, UpdateValidatorPreferencesParams,
        UpdateValidatorPreferencesPayload,
    },
    router::KnownValidatorsLoaded,
    Api, HEADER_API_KEY,
};

impl<A: Api> ProposerApi<A> {
    /// Update preferences for a batch of validators.
    ///
    /// This function accepts a list of `UpdateValidatorPreferencesPayload` objects and performs
    /// the following steps:
    /// 1. Authenticates the request using the provided API key.
    /// 2. Validates that known validator data is loaded.
    /// 3. Confirms that each validator is known and belongs to the authenticated pool.
    /// 4. Updates stored preferences for valid validators in the database.
    ///
    /// Returns `OK` if the request was valid, regardless of partial failures.
    /// Logs the number of successful and failed updates.
    #[tracing::instrument(skip_all, fields(id = %extract_request_id(&headers)), err)]
    pub async fn update_validator_preferences(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(KnownValidatorsLoaded(known_validators_loaded)): Extension<KnownValidatorsLoaded>,
        headers: HeaderMap,
        Json(request): Json<UpdateValidatorPreferencesParams>,
    ) -> Result<StatusCode, ProposerApiError> {
        if request.validators.is_empty() {
            return Err(ProposerApiError::EmptyRequest);
        }

        if !known_validators_loaded.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let start = Instant::now();

        let api_key =
            headers.get(HEADER_API_KEY).and_then(|key| key.to_str().ok()).ok_or_else(|| {
                warn!("Missing API key provided");
                ProposerApiError::InvalidApiKey
            })?;

        let pool_name = match proposer_api.db.get_validator_pool_name(api_key).await? {
            Some(pool_name) => pool_name,
            None => {
                warn!("Invalid API key provided provided");
                return Err(ProposerApiError::InvalidApiKey);
            }
        };

        debug!(pool_name, "Authenticated validator preferences update request");

        let mut updated_count = 0;
        let mut errors = 0;

        let validator_pubkeys: Vec<BlsPublicKeyBytes> =
            request.validators.iter().map(|v| v.pubkey).collect();

        let known_validators =
            proposer_api.db.check_known_validators(validator_pubkeys.clone()).await?;
        let pool_validators = proposer_api.db.get_pool_validators(&pool_name).await?;

        for validator_update in request.validators {
            match process_single_validator_update(
                &proposer_api,
                &known_validators,
                &pool_validators,
                &validator_update,
            )
            .await
            {
                Ok(_) => {
                    updated_count += 1;
                    debug!(pubkey = ?validator_update.pubkey, "Updated validator preferences");
                }
                Err(e) => {
                    errors += 1;
                    warn!(
                        pubkey = ?validator_update.pubkey,
                        error = %e,
                        "Failed to update validator preferences"
                    );
                }
            }
        }

        debug!(
            duration = ?start.elapsed(),
            pool_name,
            updated_count,
            error_count = errors,
            total_requests = updated_count + errors,
            "Completed validator preferences update"
        );

        Ok(StatusCode::OK)
    }

    /// Retrieves preferences for a single validator.
    ///
    /// This function performs the following steps:
    /// 1. Authenticates the request using the provided API key.
    /// 2. Validates that known validator data is loaded.
    /// 3. Confirms that the validator is known and registered under the pool.
    /// 4. Returns stored preferences, or defaults if none are found.
    ///
    /// Returns `ValidatorPreferences` on success, or an error if authentication
    /// or validation fails.
    #[tracing::instrument(skip_all, fields(id = %extract_request_id(&headers)), err)]
    pub async fn get_validator_preferences(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(KnownValidatorsLoaded(known_validators_loaded)): Extension<KnownValidatorsLoaded>,
        Path(pubkey): Path<BlsPublicKeyBytes>,
        headers: HeaderMap,
    ) -> Result<impl IntoResponse, ProposerApiError> {
        if !known_validators_loaded.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        debug!("New request for validator preferences.");

        let api_key =
            headers.get(HEADER_API_KEY).and_then(|key| key.to_str().ok()).ok_or_else(|| {
                warn!("Missing API key for get validator preferences");
                ProposerApiError::InvalidApiKey
            })?;

        let pool_name = match proposer_api.db.get_validator_pool_name(api_key).await? {
            Some(pool_name) => pool_name,
            None => {
                warn!("Invalid API key provided for get validator preferences");
                return Err(ProposerApiError::InvalidApiKey);
            }
        };

        let known_validators = proposer_api.db.check_known_validators(vec![pubkey]).await?;
        if !known_validators.contains(&pubkey) {
            let pubkey = BlsPublicKey::deserialize(&pubkey[..])
                .map_err(|_| ProposerApiError::InvalidPublicKey(pubkey.to_vec()))?;
            return Err(ProposerApiError::UnknownValidator(pubkey));
        }

        let pool_validators = proposer_api.db.get_pool_validators(&pool_name).await?;
        if !pool_validators.contains(&pubkey) {
            let pubkey = BlsPublicKey::deserialize(&pubkey[..])
                .map_err(|_| ProposerApiError::InvalidPublicKey(pubkey.to_vec()))?;
            return Err(ProposerApiError::ValidatorNotRegistered(pubkey));
        }

        let preferences =
            proposer_api.db.get_validator_preferences(&pubkey).await?.unwrap_or_else(|| {
                ValidatorPreferences {
                    filtering: proposer_api.validator_preferences.filtering,
                    trusted_builders: proposer_api.validator_preferences.trusted_builders.clone(),
                    header_delay: proposer_api.validator_preferences.header_delay,
                    delay_ms: proposer_api.validator_preferences.delay_ms,
                    disable_inclusion_lists: proposer_api
                        .validator_preferences
                        .disable_inclusion_lists,
                }
            });

        Ok((StatusCode::OK, Json(preferences)).into_response())
    }
}

async fn process_single_validator_update<A: Api>(
    proposer_api: &Arc<ProposerApi<A>>,
    known_validators: &HashSet<BlsPublicKeyBytes>,
    pool_validators: &[BlsPublicKeyBytes],
    update: &UpdateValidatorPreferencesPayload,
) -> Result<(), ProposerApiError> {
    if !known_validators.contains(&update.pubkey) {
        let pubkey = BlsPublicKey::deserialize(&update.pubkey[..])
            .map_err(|_| ProposerApiError::InvalidPublicKey(update.pubkey.to_vec()))?;
        return Err(ProposerApiError::UnknownValidator(pubkey));
    }

    if !pool_validators.contains(&update.pubkey) {
        let pubkey = BlsPublicKey::deserialize(&update.pubkey[..])
            .map_err(|_| ProposerApiError::InvalidPublicKey(update.pubkey.to_vec()))?;
        return Err(ProposerApiError::ValidatorNotRegistered(pubkey));
    }

    let mut current_preferences =
        proposer_api.db.get_validator_preferences(&update.pubkey).await?.unwrap_or_else(|| {
            ValidatorPreferences {
                filtering: proposer_api.validator_preferences.filtering,
                trusted_builders: proposer_api.validator_preferences.trusted_builders.clone(),
                header_delay: proposer_api.validator_preferences.header_delay,
                delay_ms: proposer_api.validator_preferences.delay_ms,
                disable_inclusion_lists: proposer_api.validator_preferences.disable_inclusion_lists,
            }
        });

    if let Some(filtering) = update.preferences.filtering {
        current_preferences.filtering = filtering;
    }

    if let Some(trusted_builders) = &update.preferences.trusted_builders {
        current_preferences.trusted_builders = Some(trusted_builders.clone());
    }

    if let Some(header_delay) = update.preferences.header_delay {
        current_preferences.header_delay = header_delay;
    }

    if let Some(delay_ms) = update.preferences.delay_ms {
        current_preferences.delay_ms = Some(delay_ms);
    }

    if let Some(_gossip_blobs) = update.preferences.gossip_blobs {
        // current_preferences.gossip_blobs = gossip_blobs;
    }

    if let Some(disable_inclusion_lists) = update.preferences.disable_inclusion_lists {
        current_preferences.disable_inclusion_lists = disable_inclusion_lists;
    }

    proposer_api.db.update_validator_preferences(&update.pubkey, &current_preferences).await?;

    Ok(())
}
