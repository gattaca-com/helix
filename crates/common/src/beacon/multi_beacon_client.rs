use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures::future::join_all;
use helix_types::{ForkName, SignedExecutionPayloadEnvelope, VersionedSignedProposal};

use crate::{
    beacon::{beacon_client::BeaconClient, error::BeaconClientError, types::BroadcastValidation},
    chain_info::ChainInfo,
    spawn_tracked,
};

#[derive(Clone)]
pub struct MultiBeaconClient {
    pub beacon_clients: Arc<Vec<Arc<BeaconClient>>>,
    pub best_index: Arc<AtomicUsize>,
}

impl MultiBeaconClient {
    pub fn new(beacon_clients: Vec<Arc<BeaconClient>>) -> Self {
        Self { beacon_clients: Arc::new(beacon_clients), best_index: Arc::new(AtomicUsize::new(0)) }
    }

    /// Returns an iterator over beacon clients starting from the best-synced one.
    pub fn beacon_clients_by_last_response(&self) -> impl Iterator<Item = &Arc<BeaconClient>> {
        let start = self.best_index.load(Ordering::Relaxed);
        self.beacon_clients[start..].iter().chain(self.beacon_clients[..start].iter())
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, BeaconClientError> {
        let mut last_error = None;
        for client in self.beacon_clients_by_last_response() {
            match client.get_chain_info().await {
                Ok(chain_info) => return Ok(chain_info),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    /// Publishes the signed beacon block to all beacon clients; returns on first success.
    pub async fn publish_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<(), BeaconClientError> {
        let handles = self
            .beacon_clients
            .iter()
            .map(|client| {
                let client = client.clone();
                let block = block.clone();
                let broadcast_validation = broadcast_validation.clone();
                spawn_tracked!(async move {
                    client.publish_block(block, broadcast_validation, fork).await
                })
            })
            .collect::<Vec<_>>();

        let mut last_error: Option<BeaconClientError> = None;
        for res in (join_all(handles).await).into_iter().flatten() {
            let already_have_content_error =
                last_error.as_ref().is_some_and(|e| e.is_block_content_error());
            match res {
                Ok(202) => {
                    if !already_have_content_error {
                        last_error = Some(BeaconClientError::BlockIntegrationFailed);
                    }
                }
                Ok(_) => return Ok(()),
                Err(err) => {
                    // Prefer block content errors: a transient error from one client
                    // must not overwrite a content rejection from another.
                    if err.is_block_content_error() || !already_have_content_error {
                        last_error = Some(err);
                    }
                }
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }

    /// Publishes the signed execution payload envelope to all beacon clients; returns on first
    /// success. Unlike `publish_block`, fans out via plain concurrent futures, not
    /// `spawn_tracked!`.
    pub async fn publish_execution_payload_envelope(
        &self,
        envelope: Arc<SignedExecutionPayloadEnvelope>,
        fork: ForkName,
    ) -> Result<(), BeaconClientError> {
        let futures = self
            .beacon_clients
            .iter()
            .map(|client| client.publish_execution_payload_envelope(envelope.clone(), fork));

        let mut last_error: Option<BeaconClientError> = None;
        for res in join_all(futures).await {
            match res {
                Ok(_) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or(BeaconClientError::BeaconNodeUnavailable))
    }
}

#[cfg(test)]
mod tests {
    use helix_types::{BlsSignature, ExecutionPayloadEnvelope};
    use httpmock::{Method::POST, MockServer};
    use reqwest::Url;

    use super::*;
    use crate::BeaconClientConfig;

    fn envelope() -> Arc<SignedExecutionPayloadEnvelope> {
        Arc::new(SignedExecutionPayloadEnvelope {
            message: ExecutionPayloadEnvelope::empty(),
            signature: BlsSignature::empty(),
        })
    }

    fn client_for(server: &MockServer) -> Arc<BeaconClient> {
        let url = Url::parse(&server.url("/")).unwrap();
        Arc::new(BeaconClient::new(BeaconClientConfig { url }))
    }

    #[tokio::test]
    async fn publish_execution_payload_envelope_returns_ok_on_first_success() {
        crate::utils::install_default_crypto_provider();
        let failing = MockServer::start();
        failing.mock(|when, then| {
            when.method(POST).path("/eth/v1/beacon/execution_payload_envelopes");
            then.status(500);
        });
        let succeeding = MockServer::start();
        succeeding.mock(|when, then| {
            when.method(POST).path("/eth/v1/beacon/execution_payload_envelopes");
            then.status(200);
        });

        let multi = MultiBeaconClient::new(vec![client_for(&failing), client_for(&succeeding)]);
        let result = multi.publish_execution_payload_envelope(envelope(), ForkName::Gloas).await;

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn publish_execution_payload_envelope_returns_err_when_all_clients_fail() {
        crate::utils::install_default_crypto_provider();
        let a = MockServer::start();
        a.mock(|when, then| {
            when.method(POST).path("/eth/v1/beacon/execution_payload_envelopes");
            then.status(500);
        });
        let b = MockServer::start();
        b.mock(|when, then| {
            when.method(POST).path("/eth/v1/beacon/execution_payload_envelopes");
            then.status(500);
        });

        let multi = MultiBeaconClient::new(vec![client_for(&a), client_for(&b)]);
        let result = multi.publish_execution_payload_envelope(envelope(), ForkName::Gloas).await;

        assert!(result.is_err(), "expected Err, got {result:?}");
    }
}
