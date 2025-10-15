use std::time::Duration;

use axum::http::HeaderMap;

use crate::{ValidatorPreferences, api::proposer_api::GetHeaderParams};

pub trait ApiProvider: Send + Sync + Clone + 'static {
    fn get_timing(
        &self,
        params: &GetHeaderParams,
        headers: &HeaderMap,
        preferences: &ValidatorPreferences,
        ms_into_slot: u64,
    ) -> Result<TimingResult, &'static str>;

    fn get_metadata(&self, headers: &HeaderMap) -> Option<String>;
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
}
