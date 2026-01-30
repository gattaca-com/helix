use std::sync::{Arc, atomic::Ordering};

use axum::{
    Extension,
    extract::Json,
    http::{HeaderMap, StatusCode},
};
use helix_common::{
    Filtering, ValidatorPreferences,
    api::proposer_api::ValidatorRegistrationInfo,
    api_provider::ApiProvider,
    metrics::{
        REGISTRATIONS_INVALID, REGISTRATIONS_SKIPPED, REGISTRATIONS_TO_CHECK_COUNT,
        REGISTRATIONS_UNKNOWN,
    },
    utils::extract_request_id,
};
use helix_types::SignedValidatorRegistration;
use tokio::{task::JoinSet, time::Instant};
use tracing::{error, info, trace, warn};

use super::ProposerApi;
use crate::api::{
    Api, HEADER_API_KEY,
    proposer::{PreferencesHeader, error::ProposerApiError},
    router::KnownValidatorsLoaded,
};

impl<A: Api> ProposerApi<A> {
    /// Registers a batch of validators to the relay.
    ///
    /// This function accepts a list of `SignedValidatorRegistration` objects and performs the
    /// following steps:
    /// 1. Validates the registration timestamp of each validator.
    /// 2. Checks if the validator is known in the validator registry.
    /// 3. Verifies the signature of each registration.
    /// 4. Writes validated registrations to the registry.
    ///
    /// If all registrations in the batch fail validation, an error is returned.
    ///
    /// Implements this API: <https://ethereum.github.io/builder-specs/#/Builder/registerValidator>
    #[tracing::instrument(skip_all, fields(id =% extract_request_id(&headers)), err)]
    pub async fn register_validators(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(KnownValidatorsLoaded(known_validators_loaded)): Extension<KnownValidatorsLoaded>,
        headers: HeaderMap,
        Json(registrations): Json<Vec<SignedValidatorRegistration>>,
    ) -> Result<StatusCode, ProposerApiError> {
        if registrations.is_empty() {
            return Err(ProposerApiError::EmptyRequest);
        }

        if !known_validators_loaded.load(Ordering::Relaxed) {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        // Get optional api key from headers
        let api_key = headers.get(HEADER_API_KEY).and_then(|key| key.to_str().ok());

        let pool_name = match api_key {
            Some(api_key) => match proposer_api.db.get_validator_pool_name(api_key).await? {
                Some(pool_name) => Some(pool_name),
                None => {
                    warn!("Invalid api key provided");
                    return Err(ProposerApiError::InvalidApiKey);
                }
            },
            None => None,
        };

        // Set using default preferences from config
        let mut validator_preferences = ValidatorPreferences {
            filtering: proposer_api.validator_preferences.filtering,
            trusted_builders: proposer_api.validator_preferences.trusted_builders.clone(),
            header_delay: proposer_api.validator_preferences.header_delay,
            delay_ms: proposer_api.validator_preferences.delay_ms,
            disable_inclusion_lists: proposer_api.validator_preferences.disable_inclusion_lists,
            disable_optimistic: proposer_api.validator_preferences.disable_optimistic,
        };

        let preferences_header = headers.get("x-preferences");

        let preferences = match preferences_header {
            Some(preferences_header) => {
                let decoded_prefs: PreferencesHeader =
                    serde_json::from_str(preferences_header.to_str()?)?;
                Some(decoded_prefs)
            }
            None => None,
        };

        if let Some(preferences) = preferences {
            // Overwrite preferences if they are provided

            if let Some(filtering) = preferences.filtering {
                validator_preferences.filtering = filtering;
            } else if let Some(censoring) = preferences.censoring {
                validator_preferences.filtering = match censoring {
                    true => Filtering::Regional,
                    false => Filtering::Global,
                };
            }

            if let Some(trusted_builders) = preferences.trusted_builders {
                validator_preferences.trusted_builders = Some(trusted_builders);
            }

            if let Some(header_delay) = preferences.header_delay {
                validator_preferences.header_delay = header_delay;
            }

            if let Some(disable_optimistic) = preferences.disable_optimistic {
                validator_preferences.disable_optimistic = disable_optimistic;
            }
        }

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let head_slot = proposer_api.curr_slot_info.head_slot();
        let num_registrations = registrations.len();
        trace!(%head_slot, num_registrations,);

        let mut unknown_registrations = 0;
        let mut skipped_registrations = 0;

        let registrations_to_check: Vec<_> = {
            let known_validators_guard = proposer_api.db.known_validators_cache().read();
            registrations
                .into_iter()
                .filter(|reg| {
                    if known_validators_guard.contains(&reg.message.pubkey) {
                        true
                    } else {
                        unknown_registrations += 1;
                        false
                    }
                })
                .filter(|reg| {
                    if proposer_api.db.is_registration_update_required(reg) {
                        true
                    } else {
                        skipped_registrations += 1;
                        false
                    }
                })
                .collect()
        };

        REGISTRATIONS_UNKNOWN.inc_by(unknown_registrations);
        REGISTRATIONS_SKIPPED.inc_by(skipped_registrations);

        if registrations_to_check.is_empty() {
            return Ok(StatusCode::OK);
        }

        // create smaller batches than n workers to allow for jitter in verification time of a
        // single batch
        let batch_size =
            (registrations_to_check.len() / proposer_api.relay_config.cores.reg_workers.len() / 8)
                .max(1);

        let registrations_to_check = Arc::new(registrations_to_check);
        let mut join_set = JoinSet::new();
        let start = Instant::now();
        for (i, batch) in registrations_to_check.chunks(batch_size).enumerate() {
            let r = i * batch_size..i * batch_size + batch.len();
            let Ok(rx) = proposer_api.reg_handle.send(registrations_to_check.clone(), r) else {
                error!("failed sending registration batch to worker");
                return Err(ProposerApiError::InternalServerError);
            };

            join_set.spawn(rx);
        }

        REGISTRATIONS_TO_CHECK_COUNT.inc_by(registrations_to_check.len() as u64);

        let mut to_process = vec![false; registrations_to_check.len()];
        let mut successful_registrations = 0;

        while let Some(res) = join_set.join_next().await {
            let Ok(Ok(res)) = res else {
                return Err(ProposerApiError::InternalServerError);
            };

            for (i, is_valid) in res {
                to_process[i] = is_valid;
                if is_valid {
                    successful_registrations += 1;
                }
            }
        }

        let process_time = start.elapsed();

        let invalid_registrations = registrations_to_check.len() - successful_registrations;
        REGISTRATIONS_INVALID.inc_by(invalid_registrations as u64);

        if successful_registrations == 0 {
            return Err(ProposerApiError::NoValidatorsCouldBeRegistered);
        }

        let registrations_to_save = Arc::unwrap_or_clone(registrations_to_check)
            .into_iter()
            .zip(to_process.iter())
            .filter_map(|(reg, to_process)| {
                if *to_process {
                    Some(ValidatorRegistrationInfo {
                        registration: reg,
                        preferences: validator_preferences.clone(),
                    })
                } else {
                    None
                }
            });

        proposer_api.db.save_validator_registrations(registrations_to_save, pool_name, user_agent);

        info!(
            ?process_time,
            num_registrations,
            successful_registrations,
            unknown_registrations,
            skipped_registrations,
            invalid_registrations,
            "processed registrations"
        );

        Ok(StatusCode::OK)
    }
}
