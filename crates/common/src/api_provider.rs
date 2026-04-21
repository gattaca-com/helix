use std::{sync::Arc, time::Duration};

use axum::http::HeaderMap;
use helix_types::SignedValidatorRegistration;
use tracing::warn;

use crate::{PreferencesHeader, ValidatorPreferences, api::proposer_api::GetHeaderParams};

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
}

pub struct TimingResult {
    pub sleep_time: Option<Duration>,
    pub is_mev_boost: bool,
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
        _headers: &HeaderMap,
        _preferences: &ValidatorPreferences,
        _ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str> {
        Ok(TimingResult { sleep_time: None, is_mev_boost: false })
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
