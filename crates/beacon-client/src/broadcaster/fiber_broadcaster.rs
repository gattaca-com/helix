use std::sync::Arc;

use ethereum_consensus::{clock::get_current_unix_time_in_nanos, Fork};
use helix_common::signed_proposal::VersionedSignedProposal;
use helix_utils::request_encoding::Encoding;
use tracing::debug;

use crate::{error::BeaconClientError, types::BroadcastValidation};

pub struct FiberBroadcaster {
    _encoding: Encoding,
    client: fiber::Client,
}

impl FiberBroadcaster {
    pub async fn new(url: String, api_key: String, encoding: Encoding) -> Result<Self, BeaconClientError> {
        tokio::time::sleep(tokio::time::Duration::from_secs(12)).await;
        let client =
            fiber::Client::connect(url, api_key).await.map_err(|err| BeaconClientError::BroadcasterInitError(format!("Fiber Err: {err}")))?;
        Ok(Self { _encoding: encoding, client })
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        _broadcast_validation: Option<BroadcastValidation>,
        _consensus_version: Fork,
    ) -> Result<(), BeaconClientError> {
        let ts_before_ssz = get_current_unix_time_in_nanos();
        match block.get_ssz_bytes_to_publish() {
            Ok(ssz_block) => {
                let ts_after_ssz = get_current_unix_time_in_nanos();
                match self.client.publish_block(ssz_block).await {
                    Ok(_) => {
                        let ts_after_publish = get_current_unix_time_in_nanos();
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
            Err(err) => Err(BeaconClientError::SszSerializationError(err)),
        }
    }

    pub fn identifier(&self) -> String {
        "FIBER".to_string()
    }
}
