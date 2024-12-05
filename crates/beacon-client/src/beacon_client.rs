use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use ethereum_consensus::{primitives::Root, ssz};
use futures::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest_eventsource::EventSource;
use tokio::{sync::broadcast::Sender, time::sleep};
use tracing::{debug, error, warn};

use helix_common::{
    beacon_api::PublishBlobsRequest, bellatrix::SimpleSerialize,
    signed_proposal::VersionedSignedProposal, BeaconClientConfig, ProposerDuty, ValidatorSummary,
};

use crate::{
    error::{ApiError, BeaconClientError},
    traits::BeaconClientTrait,
    types::{
        ApiResult, BeaconResponse, BroadcastValidation, HeadEventData, PayloadAttributesEvent,
        StateId, SyncStatus,
    },
};

const CONSENSUS_VERSION_HEADER: &str = "eth-consensus-version";

// Note: we noticed that beacon clients can take 5-10s to respond with the full
// validators list in some cases. The previous timeout of 5s was too short.
const BEACON_CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct BeaconClient {
    pub http: reqwest::Client,
    pub config: BeaconClientConfig,
}

impl BeaconClient {
    pub fn new(http: reqwest::Client, config: BeaconClientConfig) -> Self {
        Self { http, config }
    }

    pub fn from_config(config: BeaconClientConfig) -> Self {
        let client =
            reqwest::ClientBuilder::new().timeout(BEACON_CLIENT_REQUEST_TIMEOUT).build().unwrap();
        Self::new(client, config)
    }

    pub async fn get<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, BeaconClientError> {
        let result = self.http_get(path).await?.json().await?;
        match result {
            ApiResult::Ok(result) => Ok(result),
            ApiResult::Err(err) => Err(err.into()),
        }
    }

    pub async fn http_get(&self, path: &str) -> Result<reqwest::Response, BeaconClientError> {
        let target = self.config.url.join(path)?;
        Ok(self.http.get(target).send().await?)
    }

    pub async fn post<T: serde::Serialize + ?Sized>(
        &self,
        path: &str,
        argument: &T,
    ) -> Result<(), BeaconClientError> {
        let response = self.http_post(path, argument).await?;
        match response.status() {
            reqwest::StatusCode::OK | reqwest::StatusCode::ACCEPTED => Ok(()),
            _ => {
                let api_err = response.json::<ApiError>().await?;
                Err(BeaconClientError::Api(api_err))
            }
        }
    }

    pub async fn http_post<T: serde::Serialize + ?Sized>(
        &self,
        path: &str,
        argument: &T,
    ) -> Result<reqwest::Response, BeaconClientError> {
        let target = self.config.url.join(path)?;
        Ok(self.http.post(target).json(argument).send().await?)
    }

    /// Subscribe to SSE events from the beacon client `events` endpoint.
    pub async fn subscribe_to_sse<T: serde::de::DeserializeOwned>(
        &self,
        topic: &str,
        chan: Sender<T>,
    ) -> Result<(), BeaconClientError> {
        let url = format!("{}eth/v1/events?topics={}", self.config.url, topic);

        loop {
            let mut es = EventSource::get(&url);

            while let Some(event) = es.next().await {
                match event {
                    Ok(reqwest_eventsource::Event::Message(message)) => {
                        match serde_json::from_str::<T>(&message.data) {
                            Ok(data) => {
                                if chan.send(data).is_err() {
                                    debug!("no subscribers connected to sse broadcaster");
                                }
                            }
                            Err(err) => error!(err=%err, "Error parsing chunk"),
                        }
                    }
                    Ok(reqwest_eventsource::Event::Open) => {}
                    Err(err) => {
                        warn!(err=%err, "SSE stream ended, reconnecting...");
                        es.close();
                        break
                    }
                }
            }
            sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn broadcast_block(
        &self,
        block: Arc<VersionedSignedProposal>,
        broadcast_validation: Option<BroadcastValidation>,
        consensus_version: ethereum_consensus::Fork,
    ) -> Result<(), BeaconClientError> {
        self.publish_block(block, broadcast_validation, consensus_version).await?;
        Ok(())
    }

    pub fn identifier(&self) -> String {
        "BEACON-CLIENT".to_string()
    }
}

#[async_trait]
impl BeaconClientTrait for BeaconClient {
    async fn sync_status(&self) -> Result<SyncStatus, BeaconClientError> {
        let response: BeaconResponse<SyncStatus> = self.get("eth/v1/node/syncing").await?;
        Ok(response.data)
    }

    async fn current_slot(&self) -> Result<u64, BeaconClientError> {
        let sync_status = self.sync_status().await?;
        Ok(sync_status.head_slot)
    }

    async fn subscribe_to_head_events(
        &self,
        chan: Sender<HeadEventData>,
    ) -> Result<(), BeaconClientError> {
        self.subscribe_to_sse("head", chan).await
    }

    async fn subscribe_to_payload_attributes_events(
        &self,
        chan: Sender<PayloadAttributesEvent>,
    ) -> Result<(), BeaconClientError> {
        self.subscribe_to_sse("payload_attributes", chan).await
    }

    /// Fetch all known validators with an `active` status.
    async fn get_state_validators(
        &self,
        state_id: StateId,
    ) -> Result<Vec<ValidatorSummary>, BeaconClientError> {
        let endpoint = format!("eth/v1/beacon/states/{state_id}/validators?status=active,pending");
        let result: BeaconResponse<Vec<ValidatorSummary>> = self.get(&endpoint).await?;
        Ok(result.data)
    }

    async fn get_proposer_duties(
        &self,
        epoch: u64,
    ) -> Result<(Root, Vec<ProposerDuty>), BeaconClientError> {
        let endpoint = format!("eth/v1/validator/duties/proposer/{epoch}");
        let mut result: BeaconResponse<Vec<ProposerDuty>> = self.get(&endpoint).await?;
        let dependent_root_value = result.meta.remove("dependent_root").ok_or_else(|| {
            BeaconClientError::MissingExpectedData(
                "missing `dependent_root` in response".to_string(),
            )
        })?;
        let dependent_root: Root = serde_json::from_value(dependent_root_value)?;
        Ok((dependent_root, result.data))
    }

    /// `publish_block` publishes the signed beacon block ssz-encoded via
    /// <https://ethereum.github.io/beacon-APIs/#/ValidatorRequiredApi/publishBlockV2>
    async fn publish_block<SB: Send + Sync + SimpleSerialize>(
        &self,
        block: Arc<SB>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<u16, BeaconClientError> {
        let target = self.config.url.join("eth/v2/beacon/blocks")?;
        let body_bytes = ssz::prelude::serialize(block.as_ref())?;
        let mut request = self
            .http
            .post(target)
            .body(body_bytes)
            .header(CONSENSUS_VERSION_HEADER, fork.to_string())
            .header(CONTENT_TYPE, "application/octet-stream");
        if let Some(validation) = broadcast_validation {
            request = request.query(&[("broadcast_validation", validation.to_string())]);
        }
        let response = request.send().await?;

        match response.status() {
            reqwest::StatusCode::OK => Ok(response.status().as_u16()),
            reqwest::StatusCode::ACCEPTED => {
                let code = response.status().as_u16();
                let headers = response.headers().clone();
                let body = response.text().await?;

                warn!("Block accepted but not processed: {:?} body: {:?}", headers, body);
                if body.contains("duplicate block") {
                    return Ok(200)
                }
                Ok(code)
            }
            _ => {
                let api_err = response.json::<ApiError>().await?;
                Err(BeaconClientError::Api(api_err))
            }
        }
    }

    async fn publish_blobs(
        &self,
        blob_sidecars: PublishBlobsRequest,
    ) -> Result<u16, BeaconClientError> {
        if !self.config.gossip_blobs_enabled {
            return Err(BeaconClientError::RequestNotSupported)
        }

        let target = self.config.url.join("prysm/v1/beacon/blobs")?;
        let body_bytes = serde_json::to_vec(&blob_sidecars)?;
        let request =
            self.http.post(target).header(CONTENT_TYPE, "application/json").body(body_bytes);
        let response = request.send().await?;

        match response.status() {
            reqwest::StatusCode::OK | reqwest::StatusCode::ACCEPTED => {
                Ok(response.status().as_u16())
            }
            _ => {
                let api_err = response.json::<ApiError>().await?;
                Err(BeaconClientError::Api(api_err))
            }
        }
    }

    fn get_uri(&self) -> String {
        self.config.url.to_string()
    }
}

#[cfg(test)]
mod beacon_client_tests {
    use super::*;
    use mockito::Matcher;
    use tokio::sync::broadcast::channel;
    use url::Url;

    #[tokio::test]
    async fn test_get_sync_status_ok() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", Matcher::Regex("/eth/v1/node/syncing".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"is_syncing":false,"is_optimistic":false,"head_slot":"7222736","sync_distance":"1"}}"#)
            .create();

        let client = BeaconClient::from_config(BeaconClientConfig {
            url: Url::parse(&server.url()).unwrap(),
            gossip_blobs_enabled: false,
        });
        let result = client.sync_status().await;

        assert!(result.is_ok());
        let sync_status = result.unwrap();

        assert_eq!(sync_status.head_slot, 7222736);
        assert_eq!(sync_status.sync_distance, 1);
        assert!(!sync_status.is_syncing);
    }

    #[tokio::test]
    async fn test_get_state_validators_ok() {
        // Add \\ as Matcher::Regex() can't deal with "?" properly.
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", Matcher::Regex("eth/v1/beacon/states/genesis/validators\\?status=active,pending".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"execution_optimistic":false,"data":[{"index":"0","balance":"32000000000","status":"active_ongoing","validator":{"pubkey":"0x933ad9491b62059dd065b560d256d8957a8c402cc6e8d8ee7290ae11e8f7329267a8811c397529dac52ae1342ba58c95","withdrawal_credentials":"0x00f50428677c60f997aadeab24aabf7fceaef491c96a52b463ae91f95611cf71","effective_balance":"32000000000","slashed":false,"activation_eligibility_epoch":"0","activation_epoch":"0","exit_epoch":"18446744073709551615","withdrawable_epoch":"18446744073709551615"}}]}"#)
            .create();

        let client = BeaconClient::from_config(BeaconClientConfig {
            url: Url::parse(&server.url()).unwrap(),
            gossip_blobs_enabled: false,
        });
        let result = client.get_state_validators(StateId::Genesis).await;

        assert!(result.is_ok());

        let validators = result.unwrap();
        assert_eq!(validators.len(), 1);
        assert_eq!(validators[0].balance, 32000000000);
    }

    #[tokio::test]
    async fn test_get_proposer_duties_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/validator/duties/proposer/225740".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"dependent_root":"0x44bff3186a234cf4fb2799c9a44dc089e33cd976a804081c652c47a8d66f11c2","execution_optimistic":false,"data":[{"pubkey":"0x926d6d7bcd4d6066d2c8a68fd2fd07f9f9eb0ac92adb3c0ddf57b63e65c078923a7cc67a17fc38cf7acb18f37795a343","validator_index":"556715","slot":"7223680"},{"pubkey":"0x8e8663c5da817c47c98099203c402e48992c4094a7e4c9b13e5ce89213b3c46a3c71613b5b3740a855c4c45493abf7ba","validator_index":"813363","slot":"7223681"},{"pubkey":"0xac3a37ae6c8047b4b467bd978590fe99825de55184f0688b4a275b92dfbe040c41b86eed25c21d43eb5a41be7e9e8e57","validator_index":"738684","slot":"7223682"},{"pubkey":"0xb82a5e04a517bab45fd53d8878eaf30b59ec201ded0b16109f186fdc0bd9b9f97110fda24d4d6f4dc9c82106f1b1471d","validator_index":"482430","slot":"7223683"},{"pubkey":"0x96127de24ea2b357cf382f1ef6c16d0aeb57d6346b80aaf6ad1db8263cf32261cb3c8edde53bb6fd1575e6e3d3c87cbc","validator_index":"232784","slot":"7223684"},{"pubkey":"0x8ca17cd0666681c3a41645cedfe709bb05480ed22bfab4849d7d7f923bf26ae5ff6b602866158c9dc49db53e2290019f","validator_index":"515504","slot":"7223685"},{"pubkey":"0xa4730c958451373e474423858fb200bba2f27a84f0e4557d1fbd146a61a74f13ff623b5ec7ffff802e86cc56a061b988","validator_index":"723480","slot":"7223686"},{"pubkey":"0x8114af3795548eb69bfc67c5f85b2fd4c38b23b711bcf7a5982bcf2a6fead389ecc5a1ecd9fd6b28f0cb5e6804750ee2","validator_index":"583875","slot":"7223687"},{"pubkey":"0xb8e11789ba6dad3806e71c88fba1756bb800e744139beaa3240979a66d6c3545deaed0be68a60e0467e653b199fb1891","validator_index":"796168","slot":"7223688"},{"pubkey":"0xa7efad97f92d8d760d9afcdbc68a7eef1efae1226c59ab83bd149fda0a0c94a0c8a49b1ba179377c3783bd47355cdb8c","validator_index":"91207","slot":"7223689"},{"pubkey":"0x90cbf6283a34e0c31f36cd3f720db50f96b29cd07bd0c7f092d387cd0537dcd852ffa7876623e48a5d16c96d9934b949","validator_index":"180815","slot":"7223690"},{"pubkey":"0x901c339534e94975cd30791ed23de5b133e3f36a78cd1792373c994a785bf0cdefa373de5657037816c036ef0e907fa2","validator_index":"86343","slot":"7223691"},{"pubkey":"0xac66ed0a9fde6d2b965144a4951dfb92e94a6a3a2c8165118d1e77e7353da91c473cfef5e220e1d9b24b56b062a857d4","validator_index":"121110","slot":"7223692"},{"pubkey":"0x8647bedce6e3e914fd1cdccc94cba797f075222831949fe463d1017c95579afb051bd53c1b19b0b61835c7d9096c1582","validator_index":"282963","slot":"7223693"},{"pubkey":"0xaf6643b796eaf2cdcbaa26b22a54005b795f758660dd57a99e378e73b83d49218658d6d34b30e7a08d5495cf9ac5803b","validator_index":"751090","slot":"7223694"},{"pubkey":"0xb7cc47302885b62e7e86364ffcfd261125c65cc951eb456e09497165de78d59571b29e993226807e4329ccebf48a4625","validator_index":"185357","slot":"7223695"},{"pubkey":"0x998c35d2a625d59f6d2992b031faa9fba55bab5bd684d3924fd27e9e608a1d32af8c594dc9adb96c6e0092ce6e597690","validator_index":"791244","slot":"7223696"},{"pubkey":"0x906c8721c51b912ceae9bc65db098db6b826d6300525f6b45fde4878952e34d6b375940d90caf0ec58799d663b565b43","validator_index":"833436","slot":"7223697"},{"pubkey":"0x80748bda405e68d2db44e13fa233d78a870545be2477daa19bbb512ffbc2dc8141dda43e56a56e4c8059c66216182569","validator_index":"166263","slot":"7223698"},{"pubkey":"0xb98fa5876db8d897c9978cbddceb7186589b544383a4439b9fd68db692c23329b61d4b6bc580f0ab7601f5e75e14f8ce","validator_index":"563603","slot":"7223699"},{"pubkey":"0xadb3ac4a0d4430ed9de8c99617537e21e39150ab31bb7ed76973439d5adbacfcf138fa8f06612e924c456a642c2b6a26","validator_index":"11615","slot":"7223700"},{"pubkey":"0x90c486d7a798917917d7f52d69cf4660af4d13a6fc7980c7c8b6cc657e351e6c4cfe6d3cbea5cacba53f3c267484b4a8","validator_index":"533916","slot":"7223701"},{"pubkey":"0xb3dfe5db378135b3554508cab6d8929db7e533cb25ad76bc7c40c168997098e2d1c559d069d1f9704180a35d3c83bb99","validator_index":"380521","slot":"7223702"},{"pubkey":"0x81f30511b207ad23d55e19bd6de1eadbf2b02a1c26f4abac841c7b486f77481926a9f0a956485e0911d7b94ded7d55c4","validator_index":"422071","slot":"7223703"},{"pubkey":"0x9380701cec4f3f192d464018ab8042a14399f4dc2740d7c93c7b8c661345c89311b130ec279236263d92fff59a98b9e7","validator_index":"152215","slot":"7223704"},{"pubkey":"0xb49ecc067d1d8f0ca7e38d354a01d64d213244b1e0f04ca0edfdac20869280568a7957ed9ae7afd551da95f4c00eada6","validator_index":"408071","slot":"7223705"},{"pubkey":"0xb5114b89c6371ea761ca269775f8fdf45bfa904ed5cfc534c1abaa4c37614c616ee90e65971df6e8e5f5cf85c12ded19","validator_index":"835667","slot":"7223706"},{"pubkey":"0xa438628252ae10121ca0765af539d9e03d75a578e8cd9eae384ef7290564a01d6f855e6fd43916929fea05b81327483d","validator_index":"238131","slot":"7223707"},{"pubkey":"0xb89a94670d852ec5431e51e0c2099d20f1267522923e2330f1b949d62818b4ddbf2eed65d60ecdbfa22c6a3b158889af","validator_index":"515979","slot":"7223708"},{"pubkey":"0xb5cb9c936b791b529592f715fb3c38b0ad4c14f1bf883a18a31e1f18e414304a445e52ffb9b4c80663fa2f943eb6716c","validator_index":"244183","slot":"7223709"},{"pubkey":"0xa4434c1c46c35726d5964e1d514b7b663ae67bb197cc7b343f219910e2a4a272752c4ccdf2de022d50a3f0b9cead611b","validator_index":"573925","slot":"7223710"},{"pubkey":"0xaf832097555d8b8d8c326a5cb9871bf5fd2366c1ea4ae3d595cd38eb32b8ea8d817bb8e504ec01b96481362121618e7f","validator_index":"62581","slot":"7223711"}]}"#)
            .create();

        let client = BeaconClient::from_config(BeaconClientConfig {
            url: Url::parse(&server.url()).unwrap(),
            gossip_blobs_enabled: false,
        });
        let result = client.get_proposer_duties(225740).await;

        assert!(result.is_ok());
        let (node, proposer_duties) = result.unwrap();
        assert_eq!(
            format!("{node:?}"),
            "0x44bff3186a234cf4fb2799c9a44dc089e33cd976a804081c652c47a8d66f11c2"
        );
        assert_eq!(proposer_duties.len(), 32);
    }

    #[tokio::test]
    async fn test_publish_block_ok() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", Matcher::Regex("/eth/v2/beacon/blocks".to_string()))
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_header("broadcast_validation", "consensus_and_equivocation")
            .with_body(r#""#)
            .create();

        let client = BeaconClient::from_config(BeaconClientConfig {
            url: Url::parse(&server.url()).unwrap(),
            gossip_blobs_enabled: false,
        });

        let test_block = VersionedSignedProposal::default();
        let result = client
            .publish_block(
                test_block.into(),
                Some(BroadcastValidation::ConsensusAndEquivocation),
                ethereum_consensus::Fork::Capella,
            )
            .await;
        assert!(result.is_ok());

        let code = result.unwrap();
        assert_eq!(code, 200);
    }

    #[tokio::test]
    #[ignore]
    async fn test_subscribe_to_head_events_live() {
        let client = get_test_client();
        let (tx, mut rx) = channel::<HeadEventData>(1);

        tokio::spawn(async move {
            match client.subscribe_to_head_events(tx).await {
                Ok(_) => println!("Subscription completed"),
                Err(err) => panic!("Failed to subscribe to head events: {:?}", err),
            }
        });

        loop {
            match rx.recv().await {
                Ok(head_event) => {
                    println!("Passed: {:?}", head_event);
                    return
                }
                Err(err) => {
                    println!("Error: {:?}", err);
                }
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_subscribe_to_payload_events_live() {
        let client = get_test_client();
        let (tx, mut rx) = channel::<PayloadAttributesEvent>(1);

        tokio::spawn(async move {
            match client.subscribe_to_payload_attributes_events(tx).await {
                Ok(_) => println!("Subscription completed"),
                Err(err) => panic!("Failed to subscribe to head events: {:?}", err),
            }
        });

        loop {
            match rx.recv().await {
                Ok(head_event) => {
                    println!("Passed: {:?}", head_event);
                    return
                }
                Err(err) => {
                    println!("Error: {:?}", err);
                }
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_get_live_response() {
        let client = get_test_client();
        match client.sync_status().await {
            Ok(status) => {
                println!("Passed: {:?}", status);
            }
            Err(err) => {
                println!("Failed: {:?}", err);
            }
        }
    }

    fn get_test_client() -> BeaconClient {
        let config = BeaconClientConfig {
            url: Url::parse("http://localhost:5052").unwrap(),
            gossip_blobs_enabled: true,
        };
        BeaconClient::from_config(config)
    }
}
