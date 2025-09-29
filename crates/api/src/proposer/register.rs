use std::sync::{atomic::Ordering, Arc};

use axum::{
    extract::Json,
    http::{HeaderMap, StatusCode},
    Extension,
};
use helix_common::{
    api::proposer_api::ValidatorRegistrationInfo,
    chain_info::ChainInfo,
    metadata_provider::MetadataProvider,
    task,
    utils::{extract_request_id, utcnow_sec},
    Filtering, ValidatorPreferences,
};
use helix_database::DatabaseService;
use helix_types::SignedValidatorRegistration;
use tokio::time::Instant;
use tracing::{debug, error, trace, warn};

use super::ProposerApi;
use crate::{
    proposer::{error::ProposerApiError, PreferencesHeader},
    router::KnownValidatorsLoaded,
    Api, HEADER_API_KEY,
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

        let start = Instant::now();

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
        }

        let user_agent = proposer_api.metadata_provider.get_metadata(&headers);

        let head_slot = proposer_api.curr_slot_info.head_slot();
        let num_registrations = registrations.len();
        trace!(%head_slot, num_registrations,);

        // Bulk check if the validators are known
        let registration_pub_keys = registrations.iter().map(|r| r.message.pubkey).collect();
        let known_pub_keys = proposer_api.db.check_known_validators(registration_pub_keys).await?;

        // Check each registration
        let mut registrations_to_save = Vec::with_capacity(known_pub_keys.len());

        let mut handles = Vec::with_capacity(registrations.len());

        let mut unknown_registrations = 0;
        let mut update_not_required = 0;

        for registration in registrations {
            let proposer_api_clone = proposer_api.clone();
            let start_time = Instant::now();

            let pub_key = registration.message.pubkey;

            if !known_pub_keys.contains(&pub_key) {
                unknown_registrations += 1;
                continue;
            }

            if !proposer_api_clone.db.is_registration_update_required(&registration).await? {
                update_not_required += 1;
                continue;
            }

            let handle = tokio::task::spawn_blocking(move || {
                let res = match validate_registration(&proposer_api_clone.chain_info, &registration)
                {
                    Ok(_) => Some(registration),
                    Err(err) => {
                        warn!(%err, ?pub_key, "Failed to register validator");
                        None
                    }
                };

                trace!(?pub_key, elapsed_time = %start_time.elapsed().as_nanos(),);

                res
            });
            handles.push(handle);
        }

        for handle in handles {
            let reg = handle.await.map_err(|_| ProposerApiError::InternalServerError)?;
            if let Some(reg) = reg {
                registrations_to_save.push(reg);
            }
        }

        let successful_registrations = registrations_to_save.len();

        if successful_registrations == 0 {
            if unknown_registrations == num_registrations {
                debug!(
                    duration = ?start.elapsed(),
                    unknown_registrations = unknown_registrations,
                    "all registrations were unknown"
                );
                return Err(ProposerApiError::NoValidatorsCouldBeRegistered);
            }

            if update_not_required == num_registrations {
                debug!(
                    duration = ?start.elapsed(),
                    update_not_required = update_not_required,
                    "registrations already up to date"
                );
                return Ok(StatusCode::OK);
            }

            debug!(
                duration = ?start.elapsed(),
                unknown_registrations = unknown_registrations,
                update_not_required = update_not_required,
                failed_registrations = num_registrations,
                "no registrations were accepted"
            );
            return Err(ProposerApiError::NoValidatorsCouldBeRegistered);
        }

        // Bulk write registrations to db
        let registrations_to_save_task = registrations_to_save;
        task::spawn(file!(), line!(), async move {
            // Add validator preferences to each registration
            let mut valid_registrations_infos = Vec::new();

            for reg in registrations_to_save_task {
                let preferences = validator_preferences.clone();
                valid_registrations_infos
                    .push(ValidatorRegistrationInfo { registration: reg, preferences });
            }

            if let Err(err) = proposer_api
                .db
                .save_validator_registrations(valid_registrations_infos, pool_name, user_agent)
                .await
            {
                error!(
                    %err,
                    "failed to save validator registrations",
                );
            }
        });

        debug!(
            duration = ?start.elapsed(),
            unknown_registrations = unknown_registrations,
            update_not_required = update_not_required,
            successful_registrations = successful_registrations,
            failed_registrations = num_registrations - successful_registrations,
        );

        Ok(StatusCode::OK)
    }
}

/// Validate a single registration.
pub fn validate_registration(
    chain_info: &ChainInfo,
    registration: &SignedValidatorRegistration,
) -> Result<(), ProposerApiError> {
    validate_registration_time(chain_info, registration)?;
    registration.verify_signature(chain_info.builder_domain)?;

    Ok(())
}

/// Validates the timestamp in a `SignedValidatorRegistration` message.
///
/// - Ensures the timestamp is not too early (before genesis time)
/// - Ensures the timestamp is not too far in the future (current time + 10 seconds).
fn validate_registration_time(
    chain_info: &ChainInfo,
    registration: &SignedValidatorRegistration,
) -> Result<(), ProposerApiError> {
    let registration_timestamp = registration.message.timestamp;
    let registration_timestamp_upper_bound = utcnow_sec() + 10;

    if registration_timestamp < chain_info.genesis_time_in_secs {
        return Err(ProposerApiError::TimestampTooEarly {
            timestamp: registration_timestamp,
            min_timestamp: chain_info.genesis_time_in_secs,
        });
    } else if registration_timestamp > registration_timestamp_upper_bound {
        return Err(ProposerApiError::TimestampTooFarInTheFuture {
            timestamp: registration_timestamp,
            max_timestamp: registration_timestamp_upper_bound,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, Arc};

    use axum::{http::HeaderMap, response::IntoResponse, Extension};
    use helix_beacon::multi_beacon_client::MultiBeaconClient;
    use helix_common::{
        bid_sorter::BestGetHeader, chain_info::ChainInfo, local_cache::LocalCache,
        metadata_provider::DefaultMetadataProvider, signing::RelaySigningContext,
        utils::utcnow_sec, RelayConfig, ValidatorPreferences,
    };
    use helix_database::mock_database_service::MockDatabaseService;
    use helix_housekeeper::CurrentSlotInfo;
    use helix_types::{
        BlsPublicKeyBytes, BlsSignatureBytes, SignedValidatorRegistration,
        ValidatorRegistrationData,
    };
    use http_body_util::BodyExt;
    use hyper::StatusCode;
    use tokio::sync::mpsc::channel;

    use super::*;
    use crate::{
        gossiper::grpc_gossiper::GrpcGossiperClientManager, proposer::ProposerApi,
        router::KnownValidatorsLoaded, test_utils::MockApi,
    };

    fn build_signed_registration() -> SignedValidatorRegistration {
        let registration = ValidatorRegistrationData {
            fee_recipient: Default::default(),
            gas_limit: 123456,
            timestamp: utcnow_sec(),
            pubkey: BlsPublicKeyBytes::random(),
        };

        SignedValidatorRegistration {
            message: registration,
            signature: BlsSignatureBytes::random(),
        }
    }

    #[tokio::test]
    async fn all_unknown_registrations_return_bad_request() {
        let db = Arc::new(MockDatabaseService {
            treat_all_pubkeys_as_known: false,
            registration_update_required: true,
            ..Default::default()
        });

        let proposer_api = Arc::new(ProposerApi::<MockApi>::new(
            Arc::new(LocalCache::new_test()),
            db,
            GrpcGossiperClientManager::mock().into(),
            Arc::new(DefaultMetadataProvider),
            Arc::new(RelaySigningContext::default()),
            Vec::new(),
            Arc::new(MultiBeaconClient::new(vec![])),
            Arc::new(ChainInfo::for_mainnet()),
            Arc::new(ValidatorPreferences::default()),
            RelayConfig::default(),
            channel(32).0,
            CurrentSlotInfo::new(),
            BestGetHeader::new(),
            channel(32).0,
        ));

        let result = ProposerApi::<MockApi>::register_validators(
            Extension(proposer_api),
            Extension(KnownValidatorsLoaded(Arc::new(AtomicBool::new(true)))),
            HeaderMap::new(),
            Json(vec![build_signed_registration()]),
        )
        .await;

        let err = result.expect_err("expected unknown registrations to fail");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert_eq!(body, "no validators could be registered");
    }

    #[tokio::test]
    async fn up_to_date_registrations_return_ok() {
        let db = Arc::new(MockDatabaseService {
            registration_update_required: false,
            ..Default::default()
        });

        let proposer_api = Arc::new(ProposerApi::<MockApi>::new(
            Arc::new(LocalCache::new_test()),
            db,
            GrpcGossiperClientManager::mock().into(),
            Arc::new(DefaultMetadataProvider),
            Arc::new(RelaySigningContext::default()),
            Vec::new(),
            Arc::new(MultiBeaconClient::new(vec![])),
            Arc::new(ChainInfo::for_mainnet()),
            Arc::new(ValidatorPreferences::default()),
            RelayConfig::default(),
            channel(32).0,
            CurrentSlotInfo::new(),
            BestGetHeader::new(),
            channel(32).0,
        ));

        let result = ProposerApi::<MockApi>::register_validators(
            Extension(proposer_api),
            Extension(KnownValidatorsLoaded(Arc::new(AtomicBool::new(true)))),
            HeaderMap::new(),
            Json(vec![build_signed_registration()]),
        )
        .await
        .expect("expected registrations to succeed");

        assert_eq!(result, StatusCode::OK);
    }
}
