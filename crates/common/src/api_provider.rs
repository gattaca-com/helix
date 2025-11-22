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

#[cfg(test)]
mod tests {
    use super::*;
    use helix_types::Slot;
    use alloy_primitives::B256;

    #[test]
    fn test_default_api_provider_get_metadata() {
        let provider = DefaultApiProvider;
        let headers = HeaderMap::new();
        
        let metadata = provider.get_metadata(&headers);
        assert_eq!(metadata, None);
    }

    #[test]
    fn test_default_api_provider_get_timing() {
        let provider = DefaultApiProvider;
        let params = GetHeaderParams {
            slot: 100,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let headers = HeaderMap::new();
        let preferences = ValidatorPreferences::default();
        
        let result = provider.get_timing(&params, &headers, &preferences, 1000);
        
        assert!(result.is_ok());
        let timing = result.unwrap();
        assert!(timing.sleep_time.is_none());
        assert!(!timing.is_mev_boost);
    }

    #[test]
    fn test_default_api_provider_clone() {
        let provider1 = DefaultApiProvider;
        let provider2 = provider1.clone();
        
        // Both should behave the same
        let headers = HeaderMap::new();
        assert_eq!(provider1.get_metadata(&headers), provider2.get_metadata(&headers));
    }

    #[test]
    fn test_timing_result_with_sleep_time() {
        let timing = TimingResult {
            sleep_time: Some(Duration::from_millis(500)),
            is_mev_boost: true,
        };
        
        assert_eq!(timing.sleep_time, Some(Duration::from_millis(500)));
        assert!(timing.is_mev_boost);
    }

    #[test]
    fn test_timing_result_without_sleep_time() {
        let timing = TimingResult {
            sleep_time: None,
            is_mev_boost: false,
        };
        
        assert_eq!(timing.sleep_time, None);
        assert!(!timing.is_mev_boost);
    }

}
