use std::{sync::Arc, task::Poll};

use ::ssz::Encode;
use alloy_primitives::B256;
use helix_types::{ForkName, LhConfig, VersionedSignedProposal, spec_from_config};
use http::{Request, header::CONTENT_TYPE};
use http_body_util::Full;
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use tracing::warn;

#[cfg(test)]
use crate::ValidatorSummary;
use crate::{
    BeaconClientConfig,
    beacon::{
        error::{ApiError, BeaconClientError},
        types::{BeaconResponse, BroadcastValidation, SyncStatus},
    },
    chain_info::ChainInfo,
    http::client::HttpClient,
};

const CONSENSUS_VERSION_HEADER: &str = "eth-consensus-version";

#[derive(Clone, Debug)]
pub struct BeaconClient {
    pub http: HttpClient,
    pub config: BeaconClientConfig,
}

impl BeaconClient {
    pub fn new(config: BeaconClientConfig) -> Self {
        let client = HttpClient::new().unwrap();
        Self { http: client, config }
    }

    pub fn endpoint(&self) -> &str {
        self.config.url.as_str()
    }

    pub async fn sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        let response: BeaconResponse<SyncStatus> = self.get("eth/v1/node/syncing").await?;
        Ok(response.data)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, BeaconClientError> {
        let target = self.config.url.join(path)?;
        let mut pending = self.http.get(&target)?;
        loop {
            match pending.poll_json::<T>() {
                Poll::Pending => {}
                Poll::Ready(r) => {
                    return r.map_err(|e| BeaconClientError::MissingExpectedData(e.to_string()))
                }
            }
            tokio::task::yield_now().await;
        }
    }

    /// Publishes the signed beacon block SSZ-encoded.
    /// <https://ethereum.github.io/beacon-APIs/#/ValidatorRequiredApi/publishBlockV2>
    pub async fn publish_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ForkName,
    ) -> Result<u16, BeaconClientError> {
        let mut target = self.config.url.join("eth/v2/beacon/blocks")?;
        if let Some(validation) = broadcast_validation {
            target.query_pairs_mut().append_pair("broadcast_validation", &validation.to_string());
        }
        let body_bytes = Bytes::from(block.as_ssz_bytes());
        let req = Request::builder()
            .method("POST")
            .uri(target.as_str())
            .header(CONSENSUS_VERSION_HEADER, fork.to_string())
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Full::new(body_bytes))?;
        let mut pending = self.http.send(&target, req)?;

        let (status, body) = loop {
            match pending.poll_bytes() {
                Poll::Pending => {}
                Poll::Ready(Ok(r)) => break r,
                Poll::Ready(Err(e)) => {
                    return Err(BeaconClientError::MissingExpectedData(e.to_string()))
                }
            }
            tokio::task::yield_now().await;
        };

        match status {
            200 => Ok(200),
            202 => {
                let body_str = String::from_utf8_lossy(&body);
                warn!("Block accepted but not processed: {body_str}");
                if body_str.contains("duplicate block") {
                    return Ok(200);
                }
                Ok(202)
            }
            _ => {
                let api_err: ApiError = serde_json::from_slice(&body)?;
                Err(BeaconClientError::Api(api_err))
            }
        }
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, BeaconClientError> {
        let spec: BeaconResponse<LhConfig> = self.get("eth/v1/config/spec").await?;
        let spec = spec_from_config(spec.data);

        #[derive(Debug, Serialize, Deserialize)]
        struct GenesisData {
            #[serde(with = "serde_utils::quoted_u64")]
            genesis_time: u64,
            genesis_validators_root: B256,
        }

        let genesis: BeaconResponse<GenesisData> = self.get("eth/v1/beacon/genesis").await?;
        let chain_info =
            ChainInfo::new(spec, genesis.data.genesis_validators_root, genesis.data.genesis_time);

        Ok(chain_info)
    }
}
