use std::sync::{Arc, atomic::Ordering};

use axum::{
    Extension,
    extract::{Json, Query},
    http::{HeaderMap, StatusCode},
};
use helix_common::{
    PreferencesHeader,
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
use tracing::{error, info, trace};

use super::ProposerApi;
use crate::api::{Api, proposer::error::ProposerApiError, router::KnownValidatorsLoaded};

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
        Query(query_prefs): Query<PreferencesHeader>,
        Json(registrations): Json<Vec<SignedValidatorRegistration>>,
    ) -> Result<StatusCode, ProposerApiError> {
        if registrations.is_empty() {
            return Err(ProposerApiError::EmptyRequest);
        }

        if !known_validators_loaded.load(Ordering::Relaxed) {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let validator_preferences = proposer_api.api_provider.get_preferences(
            &headers,
            &query_prefs,
            proposer_api.validator_preferences.clone(),
            &registrations,
        );

        let user_agent = proposer_api.api_provider.get_metadata(&headers);

        let head_slot = proposer_api.curr_slot_info.head_slot();
        let num_registrations = registrations.len();
        trace!(%head_slot, num_registrations,);

        let mut unknown_registrations = 0;
        let mut skipped_registrations = 0;

        let registrations_to_check: Vec<_> = {
            let known_validators_guard = proposer_api.local_cache.known_validators_cache.read();
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
                    if proposer_api.local_cache.is_registration_update_required(reg) {
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

        if unknown_registrations > 0 {
            return Err(ProposerApiError::UnknownValidatorsRegistration);
        }

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

        proposer_api.local_cache.save_validator_registrations(registrations_to_save, user_agent);

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
