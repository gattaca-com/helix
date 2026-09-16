use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use axum::{Extension, Json, http::StatusCode};
use helix_common::blacklist::DisallowListPayload;
use tokio::sync::{Mutex, RwLock};
use tracing::warn;
use url::Url;

const CACHE_TTL: Duration = Duration::from_secs(300);

struct Cached {
    fetched_at: Instant,
    addresses: Arc<Vec<Address>>,
}

impl Cached {
    fn is_fresh(&self, now: Instant) -> bool {
        now.duration_since(self.fetched_at) < CACHE_TTL
    }
}

pub struct BlacklistCache {
    provider: Option<Url>,
    client: reqwest::Client,
    cached: RwLock<Option<Cached>>,
    refresh: Mutex<()>,
}

impl BlacklistCache {
    pub fn new(provider: Option<Url>) -> Self {
        Self {
            provider,
            client: reqwest::Client::new(),
            cached: RwLock::new(None),
            refresh: Mutex::new(()),
        }
    }

    pub async fn addresses(&self) -> Option<Arc<Vec<Address>>> {
        let provider = self.provider.as_ref()?;

        if let Some(fresh) = self.fresh(Instant::now()).await {
            return Some(fresh);
        }

        let _guard = self.refresh.lock().await;
        if let Some(fresh) = self.fresh(Instant::now()).await {
            return Some(fresh);
        }

        match self.fetch(provider).await {
            Ok(addresses) => Some(self.store(addresses).await),
            Err(err) => {
                warn!(%err, %provider, "blacklist fetch failed");
                self.stale().await
            }
        }
    }

    async fn fresh(&self, now: Instant) -> Option<Arc<Vec<Address>>> {
        let cached = self.cached.read().await;
        cached.as_ref().filter(|cached| cached.is_fresh(now)).map(|cached| cached.addresses.clone())
    }

    async fn stale(&self) -> Option<Arc<Vec<Address>>> {
        self.cached.read().await.as_ref().map(|cached| cached.addresses.clone())
    }

    async fn store(&self, addresses: Vec<Address>) -> Arc<Vec<Address>> {
        let addresses = Arc::new(addresses);
        *self.cached.write().await =
            Some(Cached { fetched_at: Instant::now(), addresses: addresses.clone() });
        addresses
    }

    async fn fetch(&self, provider: &Url) -> eyre::Result<Vec<Address>> {
        let response = self.client.get(provider.clone()).send().await?;
        eyre::ensure!(response.status().is_success(), "HTTP {}", response.status());
        Ok(response.json::<DisallowListPayload>().await?.into_addresses())
    }
}

pub async fn get_blacklist(
    Extension(cache): Extension<Arc<BlacklistCache>>,
) -> Result<Json<Arc<Vec<Address>>>, StatusCode> {
    match cache.addresses().await {
        Some(addresses) => Ok(Json(addresses)),
        None => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const ADDR: Address = address!("0x8589427373D6D84E98730D7795D8f6f8731FDA16");

    fn cached_at(fetched_at: Instant) -> Cached {
        Cached { fetched_at, addresses: Arc::new(vec![ADDR]) }
    }

    #[test]
    fn an_entry_within_the_ttl_is_fresh() {
        let now = Instant::now();
        let cached = cached_at(now - CACHE_TTL + Duration::from_millis(1));

        assert!(cached.is_fresh(now));
    }

    #[test]
    fn an_entry_at_the_ttl_is_stale() {
        let now = Instant::now();
        let cached = cached_at(now - CACHE_TTL);

        assert!(!cached.is_fresh(now));
    }

    #[test]
    fn the_served_payload_parses_as_a_disallow_list() {
        let served = serde_json::to_string(&vec![ADDR]).expect("the response must serialize");

        let payload: DisallowListPayload =
            serde_json::from_str(&served).expect("a consumer must read the response back");

        assert_eq!(payload.into_addresses(), vec![ADDR]);
    }
}
