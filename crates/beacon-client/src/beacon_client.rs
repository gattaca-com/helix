use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use ethereum_consensus::{phase0::Fork, primitives::Root, ssz};
use futures::StreamExt;
use http::header::CONTENT_TYPE;
use reqwest_eventsource::EventSource;
use tokio::{sync::mpsc::Sender, time::sleep};
use tracing::{error, warn};
use url::Url;

use helix_common::{ProposerDuty, ValidatorSummary, signed_proposal::VersionedSignedProposal, bellatrix::SimpleSerialize};

use crate::{
    error::{ApiError, BeaconClientError},
    traits::BeaconClientTrait,
    types::{
        ApiResult, BeaconResponse, BlockId, BroadcastValidation, GenesisDetails, HeadEventData,
        PayloadAttributesEvent, RandaoResponse, StateId, SyncStatus,
    },
};

const CONSENSUS_VERSION_HEADER: &str = "eth-consensus-version";

#[derive(Clone, Debug)]
pub struct BeaconClient {
    pub http: reqwest::Client,
    pub endpoint: Url,
}

impl BeaconClient {
    pub fn new(http: reqwest::Client, endpoint: Url) -> Self {
        Self { http, endpoint }
    }

    pub fn from_endpoint_str(endpoint: &str) -> Self {
        let endpoint = Url::parse(endpoint).unwrap();
        let client = reqwest::Client::new();
        Self::new(client, endpoint)
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
        let target = self.endpoint.join(path)?;
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
        let target = self.endpoint.join(path)?;
        Ok(self.http.post(target).json(argument).send().await?)
    }

    /// Subscribe to SSE events from the beacon client `events` endpoint.
    pub async fn subscribe_to_sse<T: serde::de::DeserializeOwned>(
        &self,
        topic: &str,
        chan: Sender<T>,
    ) -> Result<(), BeaconClientError> {
        let url = format!("{}eth/v1/events?topics={}", self.endpoint, topic);

        loop {
            let mut es = EventSource::get(&url);

            while let Some(event) = es.next().await {
                match event {
                    Ok(reqwest_eventsource::Event::Message(message)) => {
                        match serde_json::from_str::<T>(&message.data) {
                            Ok(data) => {
                                chan.send(data)
                                    .await
                                    .map_err(|_| BeaconClientError::ChannelError)?;
                            }
                            Err(err) => error!(err=%err, "Error parsing chunk"),
                        }
                    }
                    Ok(reqwest_eventsource::Event::Open) => {}
                    Err(err) => {
                        warn!(err=%err, "SSE stream ended, reconnecting...");
                        es.close();
                        break;
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
    /// https://ethereum.github.io/beacon-APIs/#/ValidatorRequiredApi/publishBlockV2
    async fn publish_block<SB: Send + Sync + SimpleSerialize>(
        &self,
        block: Arc<SB>,
        broadcast_validation: Option<BroadcastValidation>,
        fork: ethereum_consensus::Fork,
    ) -> Result<u16, BeaconClientError> {
        let target = self.endpoint.join("eth/v2/beacon/blocks")?;
        let body_bytes = ssz::prelude::serialize(block.as_ref())?;
        let mut request =
            self.http
            .post(target)
            .body(body_bytes)
            .header(CONSENSUS_VERSION_HEADER, fork.to_string())
            .header(CONTENT_TYPE, "application/octet-stream");
        if let Some(validation) = broadcast_validation {
            request = request.query(&[("broadcast_validation", validation.to_string())]);
        }
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

    async fn get_genesis(&self) -> Result<GenesisDetails, BeaconClientError> {
        let details: BeaconResponse<GenesisDetails> = self.get("eth/v1/beacon/genesis").await?;
        Ok(details.data)
    }

    async fn get_spec(&self) -> Result<HashMap<String, String>, BeaconClientError> {
        let result: BeaconResponse<HashMap<String, String>> =
            self.get("eth/v1/config/spec").await?;
        Ok(result.data)
    }

    async fn get_fork_schedule(&self) -> Result<Vec<Fork>, BeaconClientError> {
        let result: BeaconResponse<Vec<Fork>> = self.get("eth/v1/config/fork_schedule").await?;
        Ok(result.data)
    }

    async fn get_block<SignedBeaconBlock: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        block_id: BlockId,
    ) -> Result<SignedBeaconBlock, BeaconClientError> {
        let result: BeaconResponse<SignedBeaconBlock> =
            self.get(&format!("eth/v2/beacon/blocks/{block_id}")).await?;
        Ok(result.data)
    }

    async fn get_randao(&self, id: StateId) -> Result<RandaoResponse, BeaconClientError> {
        let endpoint = format!("eth/v1/beacon/states/{id}/randao");
        let result: BeaconResponse<RandaoResponse> = self.get(&endpoint).await?;
        Ok(result.data)
    }

    fn get_uri(&self) -> String {
        self.endpoint.to_string()
    }
}

#[cfg(test)]
mod beacon_client_tests {
    use super::*;
    use ethereum_consensus::capella;
    use mockito::Matcher;
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn test_get_sync_status_ok() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", Matcher::Regex("/eth/v1/node/syncing".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"is_syncing":false,"is_optimistic":false,"head_slot":"7222736","sync_distance":"1"}}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.sync_status().await;

        assert!(result.is_ok());
        let sync_status = result.unwrap();

        assert_eq!(sync_status.head_slot, 7222736);
        assert_eq!(sync_status.sync_distance, 1);
        assert_eq!(sync_status.is_syncing, false);
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

        let client = BeaconClient::from_endpoint_str(&server.url());
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

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_proposer_duties(225740).await;

        assert!(result.is_ok());
        let (node, proposer_duties) = result.unwrap();
        assert_eq!(
            node.to_string(),
            "0x44bff3186a234cf4fb2799c9a44dc089e33cd976a804081c652c47a8d66f11c2"
        );
        assert_eq!(proposer_duties.len(), 32);
    }

    #[tokio::test]
    async fn test_publish_block_ok() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", Matcher::Regex("/eth/v2/beacon/blocks".to_string()))
            .with_status(20)
            .with_header("content-type", "application/json")
            .with_body(r#""#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());

        let block_str = r#"{"version":"capella","execution_optimistic":false,"data":{"message":{"slot":"7222896","proposer_index":"97377","parent_root":"0xf1009b1ca7be5f9ff2b47402e47ef876641e8f4e479ff21826476663d018cbed","state_root":"0xf0dd85c94810e2c5b606c4c337b9a8b961b45196cda3bd656db8d669189036f5","body":{"randao_reveal":"0x851269f30259309d142fd5d4d473e5bfd9950b5d1c664a575ac041ac963748351f605ce087952de12ec64e67b4f06f3d19c4718eef0941e8b384eec02aae27516422c8ef341a7f65b2d66d6d6c91b54b2f9d0fff9e4a45fd88ab02097abb2414","eth1_data":{"deposit_root":"0xb0476286e5cb428531b4d941958a3ae3c5ea01eeb773a9d3c3fd83f097c44afb","deposit_count":"940963","block_hash":"0x4b6baf5e565b201d5fdf44a4a9a94687116b0795cbe8ce2072cf64d70369bcc0"},"graffiti":"0x5374616b65204561726e2052656c617820f09f90a0207374616b656669736800","proposer_slashings":[],"attester_slashings":[],"attestations":[{"aggregation_bits":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f","data":{"slot":"7222895","index":"56","beacon_block_root":"0xf1009b1ca7be5f9ff2b47402e47ef876641e8f4e479ff21826476663d018cbed","source":{"epoch":"225714","root":"0xd970dfc62b9e78ed3069ceec0f6b838fb0a752e3e622a9236c3d810983251023"},"target":{"epoch":"225715","root":"0xc10fda82caed2d402c2345fd0ce9fab0da74344d1e8a19fe65c3861aa0a92fba"}},"signature":"0x8886f82260f48f426007fdfe833d11b09e8ececfb6d8fcebf3efeac9f01ba77a452cb0cbc2e314276ec6669ffdc5a1c0136d3b40ef0dc027139d5dbbf9e65277476dde65a7229496c88059da49159973ca21b96537efc0d2356fa077b407fc23"},{"aggregation_bits":"0x0000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000040","data":{"slot":"7222889","index":"30","beacon_block_root":"0x76e1aeee592b284cd631b451d1d72494daefd41e7d12824c0e1d43965d9d7283","source":{"epoch":"225714","root":"0xd970dfc62b9e78ed3069ceec0f6b838fb0a752e3e622a9236c3d810983251023"},"target":{"epoch":"225715","root":"0xc10fda82caed2d402c2345fd0ce9fab0da74344d1e8a19fe65c3861aa0a92fba"}},"signature":"0xa3c6f0f37b5bb7c63443ee15c0f6804ac6619f746f2385968917013839ca4d825a1efbd5a5d5499dae874e5d689884f70e8aaf5b9af40ac914968aac93650a338534bb6f1754b13fabe55d932277bcd8697c8c0e19df5315b5c523b6a7d4342c"}],"deposits":[],"voluntary_exits":[],"sync_aggregate":{"sync_committee_bits":"0xfffffffffffffffffffffffffff7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7ffffffffffffffffffffffffffff","sync_committee_signature":"0x8af1744b4f9de7c56b4fd2149e3de7843d8ca5c52bb79d8fc68aacf951c23ca902dfe74da26bd6f29a5f122507c2277c0c109026bb238888acd20db66a102ff0af2ddda1fcda97f013b66b94d13ca219dbd88da5b26eca5a6e7558bf7af0e6e8"},"execution_payload":{"parent_hash":"0x4eafb14dfc9bb7550ca92513a8a25b1b424c189c662dd490b48b60c4dcd8ae2a","fee_recipient":"0x1f9090aae28b8a3dceadf281b0f12828e676c326","state_root":"0x6e30a2ba68ebf6534d8321eb037f9f3cdfd1494a7751baf419bd6c2b98d1876f","receipts_root":"0x7121144a0c20f843b2d3109b770cfaa039b912c8b1f75db5267dbeb9a4115ac2","logs_bloom":"0x9cb1c292c11972a251045810c3ca9aea4d0020218003726629690445fd3b2470c4453316b20022597238ed6253302106973d02f2dea721019f3227321f6dd0206802528e4125cb2eaa8c5b8f50aa6a3083450efa05443a3a181e59d3cceb9e5a00d1ed04069a9a734400605811db2c4c42540a32ec1f8d8507247db6084a0065cd81b573c90fd920b46ca0b969009ce6430434234da00d1d165530cc24da1d3ccfaa4b470388e1e1204d10ce19d04490354c48bc11e92ace4860463b720d09c95dc0103f448ce65ad3b10c2fcd7674d4e0f10547250810154e54f3e668b363cc14587e629862224284c4976288a2087682e8f45151e88cc309e8b912ab0bce45","prev_randao":"0x9f9197e9f0284db1e29376ea0819091d587743d879d46db910477d3cdce99b87","block_number":"18035707","gas_limit":"30000000","gas_used":"15971176","timestamp":"1693498775","extra_data":"0x7273796e632d6275696c6465722e78797a","base_fee_per_gas":"38847930295","block_hash":"0x43ab8f7f090036723a5a2fe741892e46cef8a2b97acc3bb9997d1a7083cbe4c0","transactions":["0x02f901740181b0851ea8e4ab1e851ea8e4ab1e83022b949451c72848c68a965f66fa7a88855f9f7784502a7f80b90104771d503f000000000000000000000000000000000000000000000000000000000000000000000000000000000000000088e6a0c2ddd26feeb64f039a2c41296fcb3f5640000000000000000000000000000000000000000000000004623f9fc0647cc0000000000000000000000000000000000000000000000000000000001fb4e8af50000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000014c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000c001a0993de2a53fea31bc918b99c5a6b864ebb83f7a26a053a337bb1462359d2ca03ca0658455d62c3b2d924f372a839432f4effb0815decbf186fcfe8d94f8d1c84e69","0x02f870014b8402faf0808509a6219a0783029ecb94b78c467206c6fe9a4bf0a3281ed65d8f1b2c78e380844e71d92dc001a03bcdbe98862c396a3b0ef5bc9733e1b6052c1261c26ec2a94d910e96bd78588da038fa2754ec67f19be9068a1be290dd48266e032750a071eed5c091eec2be8b03","0x02f87101830392308085090b845fb782522994ffee087852cb4898e6c3532e776e68bc68b1143b87901bbce9b23f0880c001a0070a87ae9c98e312d6fc9d51a44e27b02d7034fa9efe9d764b7d08dc1bae36c8a06b7894d340857351e26066d16ca28fbb16612bc8d73b30edf901a422799eda25"],"withdrawals":[{"index":"16012063","validator_index":"508528","address":"0xc436eb8aed128275c8f224de2f1dd202c0ab5830","amount":"15605289"},{"index":"16012064","validator_index":"508529","address":"0xc436eb8aed128275c8f224de2f1dd202c0ab5830","amount":"15593371"}]},"bls_to_execution_changes":[]}},"signature":"0x8842473cb4159b6dc9c3485f364dce92b8f22943105458fe1a03a82d649d2f1f823b38055516697dd41f8b7be23d50a614efcf6cbacf96bd00d3bcbafe4e0847246bc866a1d1e73140e1aea76a376a64364d3573643c7b219b128861d5b00fea"}}"#;
        let test_block =
            serde_json::from_str::<capella::minimal::SignedBeaconBlock>(block_str).unwrap();

        let result =
            client.publish_block(test_block.into(), None, ethereum_consensus::Fork::Capella).await;
        assert!(result.is_ok());

        let code = result.unwrap();
        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn test_get_genesis_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/beacon/genesis".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"genesis_time":"1606824023","genesis_validators_root":"0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95","genesis_fork_version":"0x00000000"}}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_genesis().await;

        assert!(result.is_ok());
        let genesis_details = result.unwrap();

        assert_eq!(genesis_details.genesis_time, 1606824023);
        assert_eq!(
            genesis_details.genesis_validators_root.to_string(),
            "0x4b363db94e286120d76eb905340fdd4e54bfe9f06bf33ff6cf5ad27f511bfe95"
        );
        assert_eq!(genesis_details.genesis_fork_version, [0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn test_get_spec_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/config/spec".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":{"CONFIG_NAME":"mainnet","PRESET_BASE":"mainnet","TERMINAL_TOTAL_DIFFICULTY":"58750000000000000000000","TERMINAL_BLOCK_HASH":"0x0000000000000000000000000000000000000000000000000000000000000000","TERMINAL_BLOCK_HASH_ACTIVATION_EPOCH":"18446744073709551615","SAFE_SLOTS_TO_IMPORT_OPTIMISTICALLY":"128","MIN_GENESIS_ACTIVE_VALIDATOR_COUNT":"16384","MIN_GENESIS_TIME":"1606824000","GENESIS_FORK_VERSION":"0x00000000","GENESIS_DELAY":"604800","ALTAIR_FORK_VERSION":"0x01000000","ALTAIR_FORK_EPOCH":"74240","BELLATRIX_FORK_VERSION":"0x02000000","BELLATRIX_FORK_EPOCH":"144896","CAPELLA_FORK_VERSION":"0x03000000","CAPELLA_FORK_EPOCH":"194048","SECONDS_PER_SLOT":"12","SECONDS_PER_ETH1_BLOCK":"14","MIN_VALIDATOR_WITHDRAWABILITY_DELAY":"256","SHARD_COMMITTEE_PERIOD":"256","ETH1_FOLLOW_DISTANCE":"2048","INACTIVITY_SCORE_BIAS":"4","INACTIVITY_SCORE_RECOVERY_RATE":"16","EJECTION_BALANCE":"16000000000","MIN_PER_EPOCH_CHURN_LIMIT":"4","CHURN_LIMIT_QUOTIENT":"65536","PROPOSER_SCORE_BOOST":"40","DEPOSIT_CHAIN_ID":"1","DEPOSIT_NETWORK_ID":"1","DEPOSIT_CONTRACT_ADDRESS":"0x00000000219ab540356cbb839cbe05303d7705fa","MAX_COMMITTEES_PER_SLOT":"64","TARGET_COMMITTEE_SIZE":"128","MAX_VALIDATORS_PER_COMMITTEE":"2048","SHUFFLE_ROUND_COUNT":"90","HYSTERESIS_QUOTIENT":"4","HYSTERESIS_DOWNWARD_MULTIPLIER":"1","HYSTERESIS_UPWARD_MULTIPLIER":"5","SAFE_SLOTS_TO_UPDATE_JUSTIFIED":"8","MIN_DEPOSIT_AMOUNT":"1000000000","MAX_EFFECTIVE_BALANCE":"32000000000","EFFECTIVE_BALANCE_INCREMENT":"1000000000","MIN_ATTESTATION_INCLUSION_DELAY":"1","SLOTS_PER_EPOCH":"32","MIN_SEED_LOOKAHEAD":"1","MAX_SEED_LOOKAHEAD":"4","EPOCHS_PER_ETH1_VOTING_PERIOD":"64","SLOTS_PER_HISTORICAL_ROOT":"8192","MIN_EPOCHS_TO_INACTIVITY_PENALTY":"4","EPOCHS_PER_HISTORICAL_VECTOR":"65536","EPOCHS_PER_SLASHINGS_VECTOR":"8192","HISTORICAL_ROOTS_LIMIT":"16777216","VALIDATOR_REGISTRY_LIMIT":"1099511627776","BASE_REWARD_FACTOR":"64","WHISTLEBLOWER_REWARD_QUOTIENT":"512","PROPOSER_REWARD_QUOTIENT":"8","INACTIVITY_PENALTY_QUOTIENT":"67108864","MIN_SLASHING_PENALTY_QUOTIENT":"128","PROPORTIONAL_SLASHING_MULTIPLIER":"1","MAX_PROPOSER_SLASHINGS":"16","MAX_ATTESTER_SLASHINGS":"2","MAX_ATTESTATIONS":"128","MAX_DEPOSITS":"16","MAX_VOLUNTARY_EXITS":"16","INACTIVITY_PENALTY_QUOTIENT_ALTAIR":"50331648","MIN_SLASHING_PENALTY_QUOTIENT_ALTAIR":"64","PROPORTIONAL_SLASHING_MULTIPLIER_ALTAIR":"2","SYNC_COMMITTEE_SIZE":"512","EPOCHS_PER_SYNC_COMMITTEE_PERIOD":"256","MIN_SYNC_COMMITTEE_PARTICIPANTS":"1","INACTIVITY_PENALTY_QUOTIENT_BELLATRIX":"16777216","MIN_SLASHING_PENALTY_QUOTIENT_BELLATRIX":"32","PROPORTIONAL_SLASHING_MULTIPLIER_BELLATRIX":"3","MAX_BYTES_PER_TRANSACTION":"1073741824","MAX_TRANSACTIONS_PER_PAYLOAD":"1048576","BYTES_PER_LOGS_BLOOM":"256","MAX_EXTRA_DATA_BYTES":"32","MAX_BLS_TO_EXECUTION_CHANGES":"16","MAX_WITHDRAWALS_PER_PAYLOAD":"16","MAX_VALIDATORS_PER_WITHDRAWALS_SWEEP":"16384","SYNC_COMMITTEE_SUBNET_COUNT":"4","BLS_WITHDRAWAL_PREFIX":"0x00","DOMAIN_SYNC_COMMITTEE_SELECTION_PROOF":"0x08000000","DOMAIN_AGGREGATE_AND_PROOF":"0x06000000","DOMAIN_BEACON_PROPOSER":"0x00000000","RANDOM_SUBNETS_PER_VALIDATOR":"1","DOMAIN_DEPOSIT":"0x03000000","DOMAIN_SYNC_COMMITTEE":"0x07000000","DOMAIN_BEACON_ATTESTER":"0x01000000","TARGET_AGGREGATORS_PER_SYNC_SUBCOMMITTEE":"16","TARGET_AGGREGATORS_PER_COMMITTEE":"16","DOMAIN_APPLICATION_MASK":"0x00000001","EPOCHS_PER_RANDOM_SUBNET_SUBSCRIPTION":"256","DOMAIN_CONTRIBUTION_AND_PROOF":"0x09000000","DOMAIN_VOLUNTARY_EXIT":"0x04000000","DOMAIN_SELECTION_PROOF":"0x05000000","DOMAIN_RANDAO":"0x02000000"}}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_spec().await;

        assert!(result.is_ok());
        let spec = result.unwrap();

        assert!(spec.get("CONFIG_NAME").is_some());
        assert_eq!(spec.get("CONFIG_NAME").unwrap(), "mainnet");
    }

    #[tokio::test]
    async fn test_get_fork_schedule_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/config/fork_schedule".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":[{"previous_version":"0x00000000","current_version":"0x00000000","epoch":"0"},{"previous_version":"0x00000000","current_version":"0x01000000","epoch":"74240"},{"previous_version":"0x01000000","current_version":"0x02000000","epoch":"144896"},{"previous_version":"0x02000000","current_version":"0x03000000","epoch":"194048"}]}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_fork_schedule().await;

        assert!(result.is_ok());
        let fork_schedule = result.unwrap();

        assert!(fork_schedule.len() == 4);
        assert_eq!(fork_schedule[1].epoch, 74240);
    }

    #[tokio::test]
    async fn test_get_block_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v2/beacon/blocks/head".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"version":"capella","execution_optimistic":false,"data":{"message":{"slot":"7222896","proposer_index":"97377","parent_root":"0xf1009b1ca7be5f9ff2b47402e47ef876641e8f4e479ff21826476663d018cbed","state_root":"0xf0dd85c94810e2c5b606c4c337b9a8b961b45196cda3bd656db8d669189036f5","body":{"randao_reveal":"0x851269f30259309d142fd5d4d473e5bfd9950b5d1c664a575ac041ac963748351f605ce087952de12ec64e67b4f06f3d19c4718eef0941e8b384eec02aae27516422c8ef341a7f65b2d66d6d6c91b54b2f9d0fff9e4a45fd88ab02097abb2414","eth1_data":{"deposit_root":"0xb0476286e5cb428531b4d941958a3ae3c5ea01eeb773a9d3c3fd83f097c44afb","deposit_count":"940963","block_hash":"0x4b6baf5e565b201d5fdf44a4a9a94687116b0795cbe8ce2072cf64d70369bcc0"},"graffiti":"0x5374616b65204561726e2052656c617820f09f90a0207374616b656669736800","proposer_slashings":[],"attester_slashings":[],"attestations":[{"aggregation_bits":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f","data":{"slot":"7222895","index":"56","beacon_block_root":"0xf1009b1ca7be5f9ff2b47402e47ef876641e8f4e479ff21826476663d018cbed","source":{"epoch":"225714","root":"0xd970dfc62b9e78ed3069ceec0f6b838fb0a752e3e622a9236c3d810983251023"},"target":{"epoch":"225715","root":"0xc10fda82caed2d402c2345fd0ce9fab0da74344d1e8a19fe65c3861aa0a92fba"}},"signature":"0x8886f82260f48f426007fdfe833d11b09e8ececfb6d8fcebf3efeac9f01ba77a452cb0cbc2e314276ec6669ffdc5a1c0136d3b40ef0dc027139d5dbbf9e65277476dde65a7229496c88059da49159973ca21b96537efc0d2356fa077b407fc23"},{"aggregation_bits":"0x0000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000040","data":{"slot":"7222889","index":"30","beacon_block_root":"0x76e1aeee592b284cd631b451d1d72494daefd41e7d12824c0e1d43965d9d7283","source":{"epoch":"225714","root":"0xd970dfc62b9e78ed3069ceec0f6b838fb0a752e3e622a9236c3d810983251023"},"target":{"epoch":"225715","root":"0xc10fda82caed2d402c2345fd0ce9fab0da74344d1e8a19fe65c3861aa0a92fba"}},"signature":"0xa3c6f0f37b5bb7c63443ee15c0f6804ac6619f746f2385968917013839ca4d825a1efbd5a5d5499dae874e5d689884f70e8aaf5b9af40ac914968aac93650a338534bb6f1754b13fabe55d932277bcd8697c8c0e19df5315b5c523b6a7d4342c"}],"deposits":[],"voluntary_exits":[],"sync_aggregate":{"sync_committee_bits":"0xfffffffffffffffffffffffffff7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7ffffffffffffffffffffffffffff","sync_committee_signature":"0x8af1744b4f9de7c56b4fd2149e3de7843d8ca5c52bb79d8fc68aacf951c23ca902dfe74da26bd6f29a5f122507c2277c0c109026bb238888acd20db66a102ff0af2ddda1fcda97f013b66b94d13ca219dbd88da5b26eca5a6e7558bf7af0e6e8"},"execution_payload":{"parent_hash":"0x4eafb14dfc9bb7550ca92513a8a25b1b424c189c662dd490b48b60c4dcd8ae2a","fee_recipient":"0x1f9090aae28b8a3dceadf281b0f12828e676c326","state_root":"0x6e30a2ba68ebf6534d8321eb037f9f3cdfd1494a7751baf419bd6c2b98d1876f","receipts_root":"0x7121144a0c20f843b2d3109b770cfaa039b912c8b1f75db5267dbeb9a4115ac2","logs_bloom":"0x9cb1c292c11972a251045810c3ca9aea4d0020218003726629690445fd3b2470c4453316b20022597238ed6253302106973d02f2dea721019f3227321f6dd0206802528e4125cb2eaa8c5b8f50aa6a3083450efa05443a3a181e59d3cceb9e5a00d1ed04069a9a734400605811db2c4c42540a32ec1f8d8507247db6084a0065cd81b573c90fd920b46ca0b969009ce6430434234da00d1d165530cc24da1d3ccfaa4b470388e1e1204d10ce19d04490354c48bc11e92ace4860463b720d09c95dc0103f448ce65ad3b10c2fcd7674d4e0f10547250810154e54f3e668b363cc14587e629862224284c4976288a2087682e8f45151e88cc309e8b912ab0bce45","prev_randao":"0x9f9197e9f0284db1e29376ea0819091d587743d879d46db910477d3cdce99b87","block_number":"18035707","gas_limit":"30000000","gas_used":"15971176","timestamp":"1693498775","extra_data":"0x7273796e632d6275696c6465722e78797a","base_fee_per_gas":"38847930295","block_hash":"0x43ab8f7f090036723a5a2fe741892e46cef8a2b97acc3bb9997d1a7083cbe4c0","transactions":["0x02f901740181b0851ea8e4ab1e851ea8e4ab1e83022b949451c72848c68a965f66fa7a88855f9f7784502a7f80b90104771d503f000000000000000000000000000000000000000000000000000000000000000000000000000000000000000088e6a0c2ddd26feeb64f039a2c41296fcb3f5640000000000000000000000000000000000000000000000004623f9fc0647cc0000000000000000000000000000000000000000000000000000000001fb4e8af50000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000014c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000c001a0993de2a53fea31bc918b99c5a6b864ebb83f7a26a053a337bb1462359d2ca03ca0658455d62c3b2d924f372a839432f4effb0815decbf186fcfe8d94f8d1c84e69","0x02f870014b8402faf0808509a6219a0783029ecb94b78c467206c6fe9a4bf0a3281ed65d8f1b2c78e380844e71d92dc001a03bcdbe98862c396a3b0ef5bc9733e1b6052c1261c26ec2a94d910e96bd78588da038fa2754ec67f19be9068a1be290dd48266e032750a071eed5c091eec2be8b03","0x02f87101830392308085090b845fb782522994ffee087852cb4898e6c3532e776e68bc68b1143b87901bbce9b23f0880c001a0070a87ae9c98e312d6fc9d51a44e27b02d7034fa9efe9d764b7d08dc1bae36c8a06b7894d340857351e26066d16ca28fbb16612bc8d73b30edf901a422799eda25"],"withdrawals":[{"index":"16012063","validator_index":"508528","address":"0xc436eb8aed128275c8f224de2f1dd202c0ab5830","amount":"15605289"},{"index":"16012064","validator_index":"508529","address":"0xc436eb8aed128275c8f224de2f1dd202c0ab5830","amount":"15593371"}]},"bls_to_execution_changes":[]}},"signature":"0x8842473cb4159b6dc9c3485f364dce92b8f22943105458fe1a03a82d649d2f1f823b38055516697dd41f8b7be23d50a614efcf6cbacf96bd00d3bcbafe4e0847246bc866a1d1e73140e1aea76a376a64364d3573643c7b219b128861d5b00fea"}}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_block::<capella::mainnet::SignedBeaconBlock>(BlockId::Head).await;

        assert!(result.is_ok());
        let block = result.unwrap();

        assert_eq!(
            block.message.parent_root.to_string(),
            "0xf1009b1ca7be5f9ff2b47402e47ef876641e8f4e479ff21826476663d018cbed"
        );
    }

    #[tokio::test]
    async fn test_get_randao_ok() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", Matcher::Regex("/eth/v1/beacon/states/7223416/randao".to_string()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"execution_optimistic":false,"data":{"randao":"0x22e2592817b653380160ad1bf9da05ce3a6bce99b4a337ba041976f99c052a38"}}"#)
            .create();

        let client = BeaconClient::from_endpoint_str(&server.url());
        let result = client.get_randao(StateId::Slot(7223416)).await;

        assert!(result.is_ok());
        let randao = result.unwrap();

        assert_eq!(
            randao.randao.to_string(),
            "0x22e2592817b653380160ad1bf9da05ce3a6bce99b4a337ba041976f99c052a38"
        );
    }

    #[tokio::test]
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
                Some(head_event) => {
                    println!("Passed: {:?}", head_event);
                    return;
                }
                None => {}
            }
        }
    }

    #[tokio::test]
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
                Some(head_event) => {
                    println!("Passed: {:?}", head_event);
                    return;
                }
                None => {}
            }
        }
    }

    #[tokio::test]
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
        BeaconClient::from_endpoint_str("http://localhost:5052")
    }
}
