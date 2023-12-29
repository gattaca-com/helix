use ethereum_consensus::{ssz, types::mainnet::SignedBeaconBlock, Fork};
use std::sync::Arc;

use helix_utils::request_encoding::Encoding;

use crate::{error::BeaconClientError, types::BroadcastValidation};

pub struct FiberBroadcaster {
    _encoding: Encoding,
    client: fiber::Client,
}

impl FiberBroadcaster {
    pub async fn new(url: String, api_key: String, encoding: Encoding) -> Self {
        let client = fiber::Client::connect(url, api_key)
            .await
            .expect("Error initializing Fibre broadcaster");
        Self { _encoding: encoding, client }
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<SignedBeaconBlock>,
        _broadcast_validation: Option<BroadcastValidation>,
        _consensus_version: Fork,
    ) -> Result<(), BeaconClientError> {
        match ssz::prelude::serialize(&(*block)) {
            Ok(ssz_block) => match self.client.publish_block(ssz_block).await {
                Ok(_) => Ok(()),
                Err(err) => Err(BeaconClientError::BlockPublishError(err.to_string())),
            },
            Err(err) => Err(BeaconClientError::SszSerializationError(err)),
        }
    }

    pub fn identifier(&self) -> String {
        "FIBER".to_string()
    }
}
