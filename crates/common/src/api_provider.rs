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

    #[test]
    fn test_get_timing_with_various_ms_into_slot() {
        let provider = DefaultApiProvider;
        let params = GetHeaderParams {
            slot: 100,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let headers = HeaderMap::new();
        let preferences = ValidatorPreferences::default();

        // Beginning of slot (0ms)
        let result = provider.get_timing(&params, &headers, &preferences, 0);
        assert!(result.is_ok());

        // Middle of slot (6000ms = 6s out of 12s)
        let result = provider.get_timing(&params, &headers, &preferences, 6000);
        assert!(result.is_ok());

        // Near end of slot (11999ms)
        let result = provider.get_timing(&params, &headers, &preferences, 11999);
        assert!(result.is_ok());

        // At slot boundary (12000ms = 12s)
        let result = provider.get_timing(&params, &headers, &preferences, 12000);
        assert!(result.is_ok());

        // Very large value (shouldn't panic)
        let result = provider.get_timing(&params, &headers, &preferences, u64::MAX);
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_timing_with_different_slots() {
        let provider = DefaultApiProvider;
        let headers = HeaderMap::new();
        let preferences = ValidatorPreferences::default();

        // Slot 0 (genesis)
        let params_slot0 = GetHeaderParams {
            slot: 0,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let result = provider.get_timing(&params_slot0, &headers, &preferences, 0);
        assert!(result.is_ok());

        // Large slot number
        let params_large = GetHeaderParams {
            slot: 10_000_000,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let result = provider.get_timing(&params_large, &headers, &preferences, 5000);
        assert!(result.is_ok());

        // u64::MAX slot
        let params_max = GetHeaderParams {
            slot: u64::MAX,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let result = provider.get_timing(&params_max, &headers, &preferences, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_metadata_with_various_headers() {
        let provider = DefaultApiProvider;

        // Empty headers
        let headers = HeaderMap::new();
        assert_eq!(provider.get_metadata(&headers), None);

        // Headers with random content
        let mut headers = HeaderMap::new();
        headers.insert("x-custom-header", "value".parse().unwrap());
        headers.insert("user-agent", "test-agent".parse().unwrap());
        assert_eq!(provider.get_metadata(&headers), None, "Default provider always returns None");

        // Many headers (headers with various content)
        let mut headers = HeaderMap::new();
        headers.insert("x-header-1", "value1".parse().unwrap());
        headers.insert("x-header-2", "value2".parse().unwrap());
        headers.insert("x-header-3", "value3".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("authorization", "Bearer token123".parse().unwrap());
        assert_eq!(provider.get_metadata(&headers), None, "Should still return None with many headers");
    }

    #[test]
    fn test_timing_result_edge_cases() {
        // Zero duration
        let zero_timing = TimingResult {
            sleep_time: Some(Duration::from_millis(0)),
            is_mev_boost: false,
        };
        assert_eq!(zero_timing.sleep_time, Some(Duration::ZERO));

        // Very large duration
        let large_timing = TimingResult {
            sleep_time: Some(Duration::from_secs(86400)), // 1 day
            is_mev_boost: true,
        };
        assert_eq!(large_timing.sleep_time.unwrap().as_secs(), 86400);

        // Nanosecond precision
        let precise_timing = TimingResult {
            sleep_time: Some(Duration::new(1, 123_456_789)),
            is_mev_boost: false,
        };
        assert_eq!(precise_timing.sleep_time.unwrap().subsec_nanos(), 123_456_789);
    }

    #[test]
    fn test_timing_result_all_combinations() {
        // All 4 combinations of Option<Duration> and bool
        let combinations = vec![
            (None, false),
            (None, true),
            (Some(Duration::from_millis(100)), false),
            (Some(Duration::from_millis(100)), true),
        ];

        for (sleep_time, is_mev_boost) in combinations {
            let timing = TimingResult { sleep_time, is_mev_boost };
            assert_eq!(timing.sleep_time, sleep_time);
            assert_eq!(timing.is_mev_boost, is_mev_boost);
        }
    }

    #[test]
    fn test_default_api_provider_is_consistent() {
        let provider = DefaultApiProvider;
        let params = GetHeaderParams {
            slot: 42,
            parent_hash: B256::ZERO,
            pubkey: Default::default(),
        };
        let headers = HeaderMap::new();
        let preferences = ValidatorPreferences::default();

        // Multiple calls should return consistent results
        let result1 = provider.get_timing(&params, &headers, &preferences, 1000);
        let result2 = provider.get_timing(&params, &headers, &preferences, 1000);
        
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        
        let timing1 = result1.unwrap();
        let timing2 = result2.unwrap();
        
        assert_eq!(timing1.sleep_time, timing2.sleep_time);
        assert_eq!(timing1.is_mev_boost, timing2.is_mev_boost);

        // get_metadata should also be consistent
        assert_eq!(provider.get_metadata(&headers), provider.get_metadata(&headers));
    }
}
