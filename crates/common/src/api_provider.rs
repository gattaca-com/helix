use std::{net::IpAddr, sync::Arc, time::Duration};

use axum::http::HeaderMap;
use helix_types::SignedValidatorRegistration;
use tracing::warn;
pub use uuid::Uuid;

use crate::{
    PreferencesHeader, SignedValidatorRegistrationEntry, ValidatorPreferences,
    api::{HEADER_FORWARDED_FOR, HEADER_TIMEOUT_MS, proposer_api::GetHeaderParams},
};

pub fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    let value = headers.get(name)?.to_str().ok()?.parse().ok()?;
    (value > 0).then_some(value)
}

pub fn header_uuid(headers: &HeaderMap, name: &str) -> Option<Uuid> {
    Uuid::parse_str(headers.get(name)?.to_str().ok()?).ok()
}

/// Client ip as seen by our load balancer. Proxies append, so only the last hop is trusted, any
/// earlier ones are client controlled.
pub fn header_ip_addr(headers: &HeaderMap) -> Option<IpAddr> {
    let forwarded = headers.get(HEADER_FORWARDED_FOR)?.to_str().ok()?;
    forwarded.rsplit(',').next()?.trim().parse().ok()
}

pub trait ApiProvider: Send + Sync + Clone + 'static {
    fn get_timing(
        &self,
        params: &GetHeaderParams,
        headers: &HeaderMap,
        preferences: &ValidatorPreferences,
        ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str>;

    fn get_metadata(&self, headers: &HeaderMap) -> Option<String>;

    fn get_preferences(
        &self,
        headers: &HeaderMap,
        query_prefs: &PreferencesHeader,
        fallback: Arc<ValidatorPreferences>,
        _registrations: &[SignedValidatorRegistration],
    ) -> ValidatorPreferences;

    fn admit_registration(
        &self,
        _resigned: bool,
        _existing: &SignedValidatorRegistrationEntry,
        _headers: &HeaderMap,
    ) -> bool {
        true
    }

    fn admit_header_stream(
        &self,
        _params: &GetHeaderParams,
        _headers: &HeaderMap,
        _registered: &ValidatorPreferences,
    ) -> Result<(), &'static str> {
        Err("header stream not available")
    }
}

pub struct TimingResult {
    pub sleep_time: Option<Duration>,
    pub is_mev_boost: bool,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct DefaultApiProvider;

impl ApiProvider for DefaultApiProvider {
    fn get_metadata(&self, _headers: &HeaderMap) -> Option<String> {
        None
    }

    fn get_timing(
        &self,
        _params: &GetHeaderParams,
        headers: &HeaderMap,
        _preferences: &ValidatorPreferences,
        _ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str> {
        Ok(TimingResult {
            sleep_time: None,
            is_mev_boost: false,
            timeout_ms: header_u64(headers, HEADER_TIMEOUT_MS),
        })
    }

    fn get_preferences(
        &self,
        headers: &HeaderMap,
        query_prefs: &PreferencesHeader,
        fallback: Arc<ValidatorPreferences>,
        _registrations: &[SignedValidatorRegistration],
    ) -> ValidatorPreferences {
        // Set using default preferences from config
        let mut validator_preferences = ValidatorPreferences {
            filtering: fallback.filtering,
            trusted_builders: fallback.trusted_builders.clone(),
            header_delay: fallback.header_delay,
            delay_ms: fallback.delay_ms,
            disable_inclusion_lists: fallback.disable_inclusion_lists,
            disable_optimistic: fallback.disable_optimistic,
            api_key: None,
        };

        let preferences_header = headers.get("x-preferences");

        let preferences = preferences_header.and_then(|h| {
            let s = match h.to_str() {
                Ok(s) => s,
                Err(e) => {
                    warn!(%e, "x-preferences header contains non-UTF8 bytes, ignoring");
                    return None;
                }
            };
            match serde_json::from_str::<PreferencesHeader>(s) {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!(%e, raw = s, "failed to parse x-preferences header, ignoring");
                    None
                }
            }
        });

        if let Some(preferences) = preferences {
            preferences.apply(&mut validator_preferences);
        }

        // Query params override (applied after header)
        query_prefs.apply(&mut validator_preferences);

        validator_preferences
    }
}
