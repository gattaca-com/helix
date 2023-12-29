use std::sync::Arc;

use ethereum_consensus::{types::mainnet::SignedBeaconBlock, Fork};
use reqwest::{self, Client};
use url::Url;

use helix_utils::request_encoding::Encoding;

use crate::{error::BeaconClientError, types::BroadcastValidation};

#[derive(Debug, Clone)]
pub struct BloxrouteBroadcaster {
    authorization_header: String,
    url: Url,
    encoding: Encoding,
    client: Client,
}

impl BloxrouteBroadcaster {
    pub fn new(
        authorization_header: String,
        base_url: &str,
        endpoint: &str,
        encoding: Encoding,
    ) -> Self {
        let url = Url::parse(&format!("{base_url}/{endpoint}")).unwrap();
        Self { authorization_header, url, encoding, client: Client::new() }
    }

    pub async fn broadcast_block(
        &self,
        _block: Arc<SignedBeaconBlock>,
        _broadcast_validation: Option<BroadcastValidation>,
        _consensus_version: Fork,
    ) -> Result<(), BeaconClientError> {
        unimplemented!()
    }

    pub fn identifier(&self) -> String {
        "BLOXROUTE".to_string()
    }
}
