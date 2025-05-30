use std::sync::Arc;

use helix_common::utils::utcnow_ns;
use helix_types::VersionedSignedProposal;
use ssz::Encode;
use tracing::debug;

use crate::{error::BeaconClientError, types::BroadcastValidation};

pub struct FiberBroadcaster {
    client: fiber::Client,
}

impl FiberBroadcaster {
    pub async fn new(url: String, api_key: String) -> Result<Self, BeaconClientError> {
        tokio::time::sleep(tokio::time::Duration::from_secs(12)).await;
        let client = fiber::Client::connect(url, api_key)
            .await
            .map_err(|err| BeaconClientError::BroadcasterInitError(format!("Fiber Err: {err}")))?;
        Ok(Self { client })
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        _broadcast_validation: Option<BroadcastValidation>,
    ) -> Result<(), BeaconClientError> {
        let ts_before_ssz = utcnow_ns();
        let ssz_block = block.as_ssz_bytes();

        let ts_after_ssz = utcnow_ns();
        match self.client.publish_block(ssz_block).await {
            Ok(_) => {
                let ts_after_publish = utcnow_ns();
                let latency_ssz = ts_after_ssz - ts_before_ssz;
                let latency_publish = ts_after_publish - ts_after_ssz;
                let latency_total = ts_after_publish - ts_before_ssz;
                debug!(
                    start = %ts_before_ssz,
                    end_ssz = %ts_after_ssz,
                    end_publish = %ts_after_publish,
                    latency_ssz = %latency_ssz,
                    latency_publish = %latency_publish,
                    latency_total = %latency_total,
                    "FiberBroadcaster: block publishing",
                );
                Ok(())
            }
            Err(err) => Err(BeaconClientError::BlockPublishError(err.to_string())),
        }
    }

    pub fn identifier(&self) -> String {
        "FIBER".to_string()
    }
}
