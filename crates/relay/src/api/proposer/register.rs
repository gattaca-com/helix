use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Extension,
    extract::Query,
    http::{HeaderMap, StatusCode},
};
use flux::timing::IngestionTime;
use helix_common::{
    PreferencesHeader, RequestTimings,
    api::proposer_api::ValidatorRegistrationInfo,
    api_provider::{ApiProvider, header_ip_addr},
    decoder::Encoding,
    local_cache::RegistrationUpdate,
    metrics::{
        REGISTRATIONS_INVALID, REGISTRATIONS_SKIPPED, REGISTRATIONS_TO_CHECK_COUNT,
        REGISTRATIONS_UNKNOWN,
    },
    utils::{extract_request_id, utcnow_ns},
};
use helix_types::{BlsPublicKeyBytes, SignedValidatorRegistration};
use ssz::Decode;
use tokio::{task::JoinSet, time::Instant};
use tracing::{error, info, trace, warn};

use super::ProposerApi;
use crate::{
    api::{Api, proposer::error::ProposerApiError, router::KnownValidatorsLoaded},
    spine::messages::{
        GetHeaderServed, ProposerRegistered, ProposerRequestKind, RegistrationOutcome,
        RegistrationsAnswered,
    },
};

/// What a `registerValidator` request has come to so far, for its telemetry.
#[derive(Default)]
struct RegistrationReport {
    answered: RegistrationsAnswered,
    /// Registrations of scheduled proposers. One sent to verification carries its index
    /// among the registrations to check, and stays `Unverified` until the result is in.
    scheduled: Vec<(ProposerRegistered, Option<usize>)>,
}

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
    #[tracing::instrument(skip_all, fields(id), err)]
    pub async fn register_validators(
        Extension(proposer_api): Extension<Arc<ProposerApi<A>>>,
        Extension(KnownValidatorsLoaded(known_validators_loaded)): Extension<KnownValidatorsLoaded>,
        Extension(timings): Extension<RequestTimings>,
        headers: HeaderMap,
        Query(query_prefs): Query<PreferencesHeader>,
        body: bytes::Bytes,
    ) -> Result<StatusCode, ProposerApiError> {
        let request_id = extract_request_id(&headers);
        tracing::Span::current().record("id", request_id.to_string());

        proposer_api.report_request(
            ProposerRequestKind::RegisterValidator,
            0,
            BlsPublicKeyBytes::ZERO,
            timings.on_receive_ns,
            request_id,
            &headers,
        );

        let mut report = RegistrationReport::default();
        report.answered.head_slot = proposer_api.curr_slot_info.head_slot().as_u64();
        report.answered.receive_ns = timings.on_receive_ns;
        report.answered.proposer_ip = GetHeaderServed::ip_octets(header_ip_addr(&headers));
        report.answered.request_id = request_id;

        let result = proposer_api
            .process_registrations(
                known_validators_loaded,
                &headers,
                query_prefs,
                &body,
                &mut report,
            )
            .await;

        let RegistrationReport { mut answered, scheduled } = report;
        answered.answered_ns = utcnow_ns();
        answered.status = match &result {
            Ok(status) => status.as_u16(),
            Err(err) => err.status_code().as_u16(),
        };
        answered.scheduled = scheduled.len() as u32;
        proposer_api.registrations_answered.produce_with_ingestion(answered, IngestionTime::now());
        for (registered, _) in scheduled {
            proposer_api
                .proposer_registered
                .produce_with_ingestion(registered, IngestionTime::now());
        }

        result
    }

    async fn process_registrations(
        &self,
        known_validators_loaded: Arc<AtomicBool>,
        headers: &HeaderMap,
        query_prefs: PreferencesHeader,
        body: &bytes::Bytes,
        report: &mut RegistrationReport,
    ) -> Result<StatusCode, ProposerApiError> {
        let proposer_api = self;
        let registrations: Vec<SignedValidatorRegistration> =
            match Encoding::from_content_type(headers) {
                Encoding::Json => serde_json::from_slice(body)?,
                Encoding::Ssz => Vec::from_ssz_bytes(body)?,
            };
        report.answered.registrations = registrations.len() as u32;
        report.answered.newest_timestamp =
            registrations.iter().map(|reg| reg.message.timestamp).max().unwrap_or_default();

        if registrations.is_empty() {
            return Err(ProposerApiError::EmptyRequest);
        }

        if !known_validators_loaded.load(Ordering::Relaxed) {
            return Err(ProposerApiError::ServiceUnavailableError);
        }

        let validator_preferences = proposer_api.api_provider.get_preferences(
            headers,
            &query_prefs,
            proposer_api.validator_preferences.clone(),
            &registrations,
        );

        let user_agent = proposer_api.api_provider.get_metadata(headers);

        let head_slot = proposer_api.curr_slot_info.head_slot();
        let scheduled_proposers = proposer_api.local_cache.scheduled_proposers(head_slot);
        let answered = report.answered;
        let scheduled = &mut report.scheduled;
        let num_registrations = registrations.len();
        trace!(%head_slot, num_registrations,);

        let mut unknown_registrations = 0;
        let mut skipped_registrations = 0;
        let mut rejected_registrations = 0;

        let registrations_to_check: Vec<_> = {
            let known_validators_guard = proposer_api.local_cache.known_validators_cache.read();
            let mut to_check = Vec::new();
            for reg in registrations {
                let outcome = if !known_validators_guard.contains(&reg.message.pubkey) {
                    unknown_registrations += 1;
                    Some(RegistrationOutcome::UnknownValidator)
                } else {
                    match proposer_api.local_cache.registration_update(
                        &reg,
                        validator_preferences.api_key,
                        headers,
                        proposer_api.api_provider.as_ref(),
                    ) {
                        RegistrationUpdate::Required => None,
                        RegistrationUpdate::Unchanged => {
                            skipped_registrations += 1;
                            Some(RegistrationOutcome::Unchanged)
                        }
                        RegistrationUpdate::Rejected => {
                            rejected_registrations += 1;
                            Some(RegistrationOutcome::Rejected)
                        }
                    }
                };

                if let Some(slot) = scheduled_proposers.get(&reg.message.pubkey) {
                    let registered = ProposerRegistered {
                        slot: slot.as_u64(),
                        receive_ns: answered.receive_ns,
                        timestamp: reg.message.timestamp,
                        gas_limit: reg.message.gas_limit,
                        proposer_pubkey: reg.message.pubkey,
                        fee_recipient: reg.message.fee_recipient,
                        proposer_ip: answered.proposer_ip,
                        request_id: answered.request_id,
                        outcome: outcome.unwrap_or_default(),
                        ..Default::default()
                    };
                    scheduled.push((registered, outcome.is_none().then_some(to_check.len())));
                }

                if outcome.is_none() {
                    to_check.push(reg);
                }
            }
            to_check
        };

        REGISTRATIONS_UNKNOWN.inc_by(unknown_registrations);
        REGISTRATIONS_SKIPPED.inc_by(skipped_registrations);
        report.answered.unknown = unknown_registrations as u32;
        report.answered.unchanged = skipped_registrations as u32;
        report.answered.rejected = rejected_registrations as u32;

        if rejected_registrations > 0 {
            warn!(rejected_registrations, "registrations refused by the api provider");
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
        report.answered.invalid = invalid_registrations as u32;
        report.answered.saved = successful_registrations as u32;
        for (registered, checked) in report.scheduled.iter_mut() {
            if let Some(i) = *checked {
                registered.outcome = if to_process[i] {
                    RegistrationOutcome::Saved
                } else {
                    RegistrationOutcome::Invalid
                };
            }
        }

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
            rejected_registrations,
            invalid_registrations,
            "processed registrations"
        );

        Ok(StatusCode::OK)
    }
}
