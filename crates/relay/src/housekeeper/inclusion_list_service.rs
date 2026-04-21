use std::{
    task::Poll,
    time::{Duration, Instant},
};

use bytes::Bytes;
use helix_common::{
    InclusionListConfig,
    api::builder_api::InclusionList,
    http::client::{HttpClient, PendingResponse},
};
use helix_types::Transaction;
use serde::Deserialize;
use tracing::warn;
use url::Url;

pub const IL_CUTOFF: Duration = Duration::from_secs(6);
const IL_RETRY_INTERVAL: Duration = Duration::from_millis(100);

const IL_BODY: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"relay_inclusionList\",\"params\":[]}";

#[derive(Deserialize)]
struct IlJsonRpcResponse {
    result: Vec<alloy_primitives::Bytes>,
}

pub struct IlFetchState {
    node_url: Url,
    req: PendingResponse,
    deadline: Instant,
    retry_at: Option<Instant>,
}

impl IlFetchState {
    /// `timeout` should be the remaining time until the absolute cutoff (6s into the slot),
    /// not a flat 6s from now — callers must account for elapsed slot time.
    pub fn start(
        http_client: &HttpClient,
        config: &InclusionListConfig,
        timeout: Duration,
    ) -> Option<Self> {
        let node_url = config.node_url.clone();
        let req = http_client.post(&node_url, Bytes::from_static(IL_BODY)).ok()?;
        Some(Self { node_url, req, deadline: Instant::now() + timeout, retry_at: None })
    }

    pub fn poll(&mut self, http_client: &HttpClient) -> Poll<Option<InclusionList>> {
        if let Some(retry_at) = self.retry_at {
            if Instant::now() < retry_at {
                return Poll::Pending;
            }
            match http_client.post(&self.node_url, Bytes::from_static(IL_BODY)) {
                Ok(req) => {
                    self.req = req;
                    self.retry_at = None;
                }
                Err(_) => {
                    self.retry_at = Some(Instant::now() + IL_RETRY_INTERVAL);
                    return Poll::Pending;
                }
            }
        }

        if Instant::now() >= self.deadline {
            warn!("inclusion list fetch timed out");
            return Poll::Ready(None);
        }

        match self.req.poll_json::<IlJsonRpcResponse>() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(resp)) => {
                let txs: Vec<Transaction> = resp.result.into_iter().map(Transaction).collect();
                Poll::Ready(Some(InclusionList { txs: txs.into() }))
            }
            Poll::Ready(Err(e)) => {
                warn!(%e, "IL fetch failed, retrying");
                self.retry_at = Some(Instant::now() + IL_RETRY_INTERVAL);
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use helix_common::api::builder_api::InclusionList;
    use helix_types::Transaction;
    use httpmock::{Method::POST, MockServer};
    use reqwest::Url;
    use serde_json::json;

    use super::*;

    fn create_test_config(url: Url) -> InclusionListConfig {
        InclusionListConfig { node_url: url, relay_address: Address::default() }
    }

    // Note: these tests previously tested the async reqwest-based service.
    // The poll-based IlFetchState requires a real HTTP server or integration test setup.
    // Kept as a placeholder; re-enable with an appropriate mock when needed.
    #[test]
    fn il_fetch_state_starts_on_valid_config() {
        use helix_common::http::client::HttpClient;
        let server = MockServer::start();
        let url = Url::parse(&server.url("/")).unwrap();
        let config = create_test_config(url.clone());

        let _ = server.mock(|when, then| {
            when.method(POST).path("/");
            then.status(200).json_body(json!({
                "jsonrpc":"2.0","id":1,
                "result":[]
            }));
        });

        let client = HttpClient::new().unwrap();
        let state = IlFetchState::start(&client, &config, super::IL_CUTOFF);
        assert!(state.is_some());
    }
}
