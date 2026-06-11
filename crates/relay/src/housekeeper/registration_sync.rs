use std::task::Poll;

use helix_common::{
    BuilderValidatorPreferences, RegistrationSyncConfig, ValidatorPreferences,
    api::proposer_api::ValidatorRegistrationInfo,
    http::client::{HttpClient, PendingResponse},
    local_cache::LocalCache,
};
use helix_types::SignedValidatorRegistration;
use serde::Deserialize;
use tracing::error;

const SYNC_USER_AGENT: &str = "registration_sync";

/// Duty entry as served by `/relay/v1/builder/validators`. `preferences` is a
/// helix extension; relays implementing only the builder spec omit it.
#[derive(Deserialize)]
pub struct UpstreamDutyEntry {
    pub entry: SignedValidatorRegistration,
    #[serde(default)]
    pub preferences: Option<BuilderValidatorPreferences>,
}

/// Fetches the upstream relay's registrations for the proposers of the current
/// and next epoch.
pub struct RegistrationSyncFetch {
    req: PendingResponse,
}

impl RegistrationSyncFetch {
    pub fn new(http_client: &HttpClient, config: &RegistrationSyncConfig) -> Option<Self> {
        let url = config.url.join("/relay/v1/builder/validators").ok()?;
        let req = http_client.get(&url).ok()?;
        Some(Self { req })
    }

    pub fn poll(&mut self) -> Poll<Vec<UpstreamDutyEntry>> {
        match self.req.poll_json::<Vec<UpstreamDutyEntry>>() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => {
                error!(%e, "registration sync fetch error");
                Poll::Ready(vec![])
            }
            Poll::Ready(Ok(entries)) => Poll::Ready(entries),
        }
    }
}

/// Stores upstream registrations in the local cache as if the proposers had
/// registered with this relay. Entries already cached with the same fee
/// recipient, gas limit and a recent enough timestamp are skipped. Returns the
/// number of new or updated registrations.
pub fn ingest_registrations(
    entries: Vec<UpstreamDutyEntry>,
    default_preferences: &ValidatorPreferences,
    local_cache: &LocalCache,
) -> usize {
    let to_save: Vec<ValidatorRegistrationInfo> = entries
        .into_iter()
        .filter(|e| local_cache.is_registration_update_required(&e.entry))
        .map(|e| ValidatorRegistrationInfo {
            registration: e.entry,
            preferences: match e.preferences {
                Some(p) => ValidatorPreferences {
                    filtering: p.filtering,
                    trusted_builders: p.trusted_builders,
                    header_delay: default_preferences.header_delay,
                    delay_ms: default_preferences.delay_ms,
                    disable_inclusion_lists: default_preferences.disable_inclusion_lists,
                    disable_optimistic: p.disable_optimistic,
                },
                None => default_preferences.clone(),
            },
        })
        .collect();

    let count = to_save.len();
    local_cache
        .save_validator_registrations(to_save.into_iter(), Some(SYNC_USER_AGENT.to_string()));
    count
}

#[cfg(test)]
mod tests {
    use helix_common::{Filtering, local_cache::LocalCache};
    use helix_types::{SignedValidatorRegistration, ValidatorRegistrationData};

    use super::*;

    fn registration(timestamp: u64) -> SignedValidatorRegistration {
        SignedValidatorRegistration {
            message: ValidatorRegistrationData { timestamp, ..Default::default() },
            signature: Default::default(),
        }
    }

    #[test]
    fn ingest_stores_new_registration_with_upstream_preferences() {
        let cache = LocalCache::new();
        let entries = vec![UpstreamDutyEntry {
            entry: registration(100),
            preferences: Some(BuilderValidatorPreferences {
                censoring: true,
                filtering: Filtering::Regional,
                trusted_builders: Some(vec!["b1".to_string()]),
                disable_optimistic: true,
            }),
        }];

        let ingested = ingest_registrations(entries, &ValidatorPreferences::default(), &cache);
        assert_eq!(ingested, 1);

        let pubkey = registration(100).message.pubkey;
        let saved = cache.validator_registration_cache.get(&pubkey).unwrap();
        assert!(saved.registration_info.preferences.filtering.is_regional());
        assert_eq!(
            saved.registration_info.preferences.trusted_builders,
            Some(vec!["b1".to_string()])
        );
        assert!(saved.registration_info.preferences.disable_optimistic);
    }

    #[test]
    fn ingest_defaults_preferences_when_upstream_omits_them() {
        let cache = LocalCache::new();
        let defaults =
            ValidatorPreferences { filtering: Filtering::Regional, ..Default::default() };
        let entries = vec![UpstreamDutyEntry { entry: registration(100), preferences: None }];

        let ingested = ingest_registrations(entries, &defaults, &cache);
        assert_eq!(ingested, 1);

        let pubkey = registration(100).message.pubkey;
        let saved = cache.validator_registration_cache.get(&pubkey).unwrap();
        assert!(saved.registration_info.preferences.filtering.is_regional());
    }

    #[test]
    fn ingest_skips_already_cached_registration() {
        let cache = LocalCache::new();
        let entries = vec![UpstreamDutyEntry { entry: registration(100), preferences: None }];
        assert_eq!(ingest_registrations(entries, &ValidatorPreferences::default(), &cache), 1);

        let same_again = vec![UpstreamDutyEntry { entry: registration(100), preferences: None }];
        assert_eq!(ingest_registrations(same_again, &ValidatorPreferences::default(), &cache), 0);
    }

    /// Live test against a real upstream relay endpoint. Run with:
    /// `REGISTRATION_SYNC_TEST_URL=http://localhost:6073 cargo test -p helix-relay live_registration_sync -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a live upstream relay endpoint"]
    fn live_registration_sync() {
        let url = std::env::var("REGISTRATION_SYNC_TEST_URL")
            .unwrap_or_else(|_| "http://localhost:6073".to_string());
        let config = RegistrationSyncConfig { url: url.parse().unwrap() };
        let http_client = HttpClient::new().unwrap();

        let mut fetch = RegistrationSyncFetch::new(&http_client, &config).expect("fetch start");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let entries = loop {
            match fetch.poll() {
                Poll::Ready(entries) => break entries,
                Poll::Pending => {
                    assert!(std::time::Instant::now() < deadline, "fetch timed out");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        };
        assert!(!entries.is_empty(), "upstream returned no duty entries");
        println!("fetched {} duty entries from {url}", entries.len());

        let pubkeys: Vec<_> = entries.iter().map(|e| e.entry.message.pubkey).collect();

        let cache = LocalCache::new();
        let ingested = ingest_registrations(entries, &ValidatorPreferences::default(), &cache);
        println!("ingested {ingested} registrations");
        assert!(ingested > 0);

        let cached = cache.get_validator_registrations_for_pub_keys(&pubkeys);
        assert_eq!(cached.len(), pubkeys.len(), "all fetched proposers should be cached");
    }

    #[test]
    fn upstream_entry_parses_helix_response() {
        // Shape served by a helix upstream (BuilderGetValidatorsResponse).
        let response = helix_common::api::builder_api::BuilderGetValidatorsResponse::from(
            helix_common::api::builder_api::BuilderGetValidatorsResponseEntry {
                slot: 123u64.into(),
                validator_index: 7,
                entry: ValidatorRegistrationInfo {
                    registration: registration(100),
                    preferences: ValidatorPreferences {
                        filtering: Filtering::Regional,
                        ..Default::default()
                    },
                },
            },
        );
        let json = serde_json::to_string(&response).unwrap();
        let entry: UpstreamDutyEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.entry.message.timestamp, 100);
        assert!(entry.preferences.unwrap().filtering.is_regional());
    }

    #[test]
    fn upstream_entry_parses_spec_only_response() {
        // Shape served by relays without the helix `preferences` extension.
        let json = r#"{
            "slot": "123",
            "validator_index": "7",
            "entry": {
                "message": {
                    "fee_recipient": "0xabcf8e0d4e9587369b2301d0790347320302cc09",
                    "gas_limit": "30000000",
                    "timestamp": "1700000000",
                    "pubkey": "0x93247f2209abcacf57b75a51dafae777f9dd38bc7053d1af526f220a7489a6d3a2753e5f3e8b1cfe39b56f43611df74a"
                },
                "signature": "0x1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505cc411d61252fb6cb3fa0017b679f8bb2305b26a285fa2737f175668d0dff91cc1b66ac1fb663c9bc59509846d6ec05345bd908eda73e670af888da41af171505"
            }
        }"#;
        let entry: UpstreamDutyEntry = serde_json::from_str(json).unwrap();
        assert!(entry.preferences.is_none());
        assert_eq!(entry.entry.message.gas_limit, 30_000_000);
    }
}
