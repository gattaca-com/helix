use std::{collections::HashMap, time::Duration};

use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{BlsPublicKey, Slot},
    serde::as_str,
};
use helix_beacon_client::{beacon_client::BeaconClient, BeaconClientTrait};
use reqwest::{Error, Response};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc::channel, time::sleep};
use tracing::{error, info};

use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    BeaconClientConfig,
};
use url::Url;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponseEntryExternal {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: SignedValidatorRegistration,
}

#[allow(unused)]
async fn fetch_validators_from_endpoint(
    url: &str,
) -> Result<Vec<BuilderGetValidatorsResponseEntryExternal>, Error> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?.json().await?;
    Ok(resp)
}

#[allow(unused)]
async fn fetch_and_aggregate_validators(
    endpoints: &[&str],
) -> Result<Vec<ValidatorRegistrationInfo>, Error> {
    let mut all_validators: HashMap<BlsPublicKey, ValidatorRegistrationInfo> = HashMap::new();
    let (tx, mut rx) =
        channel::<Result<Vec<BuilderGetValidatorsResponseEntry>, Error>>(endpoints.len());

    // Fetch validators from all endpoints
    for endpoint in endpoints.iter() {
        let tx = tx.clone();
        let endpoint = endpoint.to_string();
        tokio::spawn(async move {
            let result = fetch_validators_from_endpoint(&endpoint).await;
            // map the result to the internal type if successful
            let result = match result {
                Ok(entries) => {
                    let entries = entries
                        .into_iter()
                        .map(|entry| BuilderGetValidatorsResponseEntry {
                            slot: entry.slot,
                            validator_index: entry.validator_index,
                            entry: ValidatorRegistrationInfo {
                                registration: entry.entry,
                                preferences: Default::default(),
                            },
                        })
                        .collect();
                    Ok(entries)
                }
                Err(err) => Err(err),
            };

            tx.send(result).await.expect("channel send failed");
        });
    }

    // Collect and aggregate results
    for _ in 0..endpoints.len() {
        if let Some(result) = rx.recv().await {
            match result {
                Ok(entries) => {
                    for entry in entries {
                        let key = entry.entry.registration.message.public_key.clone();
                        all_validators.entry(key).or_insert(entry.entry.clone());
                    }
                }
                Err(err) => {
                    println!("Error fetching validators: {err}");
                }
            }
        }
    }

    Ok(all_validators.values().cloned().collect())
}

#[allow(unused)]
async fn register_validators(
    validators: Vec<SignedValidatorRegistration>,
    endpoint: &str,
) -> Result<(), Error> {
    let client = reqwest::Client::new();
    let resp = client.post(endpoint).json(&validators).send().await?;
    info!("{:?}", resp);

    Ok(())
}

#[allow(unused)]
async fn get_status(endpoint: &str) -> Result<Response, Error> {
    let client = reqwest::Client::new();
    let resp = client.get(endpoint).send().await?;
    Ok(resp)
}

#[tokio::test]
#[ignore]
async fn run() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let endpoints = vec![
        // TODO: add
    ];
    let helix_register_endpoint = ""; // TODO: add
    let register_endpoint_status = get_status(helix_register_endpoint).await;
    info!(
        ?endpoints,
        helix_register_endpoint,
        ?register_endpoint_status,
        "running registration replays..."
    );

    let beacon_client = BeaconClient::from_config(BeaconClientConfig {
        url: Url::parse("http://localhost:5052").unwrap(),
        gossip_blobs_enabled: false,
    });

    let (head_event_sender, mut head_event_receiver) =
        tokio::sync::broadcast::channel::<helix_beacon_client::types::HeadEventData>(100);

    tokio::spawn(async move {
        if let Err(err) = beacon_client.subscribe_to_head_events(head_event_sender).await {
            error!("Error subscribing to head events: {err}");
        }
    });

    let mut first_fetch_complete = false;
    // Process registrations each half epoch
    while let Ok(head_event) = head_event_receiver.recv().await {
        info!("New head event: {}", head_event.slot);
        if head_event.slot % 5 != 0 && first_fetch_complete {
            continue
        }
        first_fetch_complete = true;

        info!("Replaying validator registrations");

        match fetch_and_aggregate_validators(&endpoints).await {
            Ok(validators) => {
                let pubkeys: Vec<BlsPublicKey> =
                    validators.iter().map(|v| v.registration.message.public_key.clone()).collect();
                info!(?pubkeys, "{} validators fetched", validators.len());

                sleep(Duration::from_secs(60)).await;

                if let Err(err) = register_validators(
                    validators.into_iter().map(|v| v.registration).collect(),
                    helix_register_endpoint,
                )
                .await
                {
                    error!("Error registering validators to our relay: {err}");
                } else {
                    info!("Successfully registered validators!");
                }
            }
            Err(err) => {
                error!("Error fetching validators: {err}");
            }
        }
    }
}
