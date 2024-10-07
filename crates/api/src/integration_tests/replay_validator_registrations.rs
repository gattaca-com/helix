use std::collections::HashMap;

use ethereum_consensus::{
    builder::SignedValidatorRegistration,
    primitives::{BlsPublicKey, Slot},
    serde::as_str,
};
use helix_beacon_client::BeaconClientTrait;
use helix_common::api::{builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo};
use reqwest::Error;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::channel;

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BuilderGetValidatorsResponseEntryExternal {
    #[serde(with = "as_str")]
    pub slot: Slot,
    #[serde(with = "as_str")]
    pub validator_index: usize,
    pub entry: SignedValidatorRegistration,
}

#[allow(unused)]
async fn fetch_validators_from_endpoint(url: &str) -> Result<Vec<BuilderGetValidatorsResponseEntryExternal>, Error> {
    let client = reqwest::Client::new();
    let resp = client.get(url).send().await?.json().await?;
    Ok(resp)
}

#[allow(unused)]
async fn fetch_and_aggregate_validators(endpoints: &Vec<&str>) -> Result<Vec<ValidatorRegistrationInfo>, Error> {
    let mut all_validators: HashMap<BlsPublicKey, ValidatorRegistrationInfo> = HashMap::new();
    let (tx, mut rx) = channel::<Result<Vec<BuilderGetValidatorsResponseEntry>, Error>>(endpoints.len());

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
                            entry: ValidatorRegistrationInfo { registration: entry.entry, preferences: Default::default() },
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
async fn register_validators(validators: Vec<SignedValidatorRegistration>, endpoint: &str) -> Result<(), Error> {
    let client = reqwest::Client::new();
    let resp = client.post(endpoint).json(&validators).send().await?;
    println!("{:?}", resp);

    Ok(())
}

#[allow(unused)]
async fn get_status(endpoint: &str) -> Result<(), Error> {
    let client = reqwest::Client::new();
    let resp = client.get(endpoint).send().await?;
    println!("{:?}", resp.status());
    Ok(())
}

#[tokio::test]
#[ignore]
async fn run() {
    let endpoints = vec![
        "https://localhost/relay/v1/builder/validators",
        "https://localhost/relay/v1/builder/validators",
        "https://localhost/relay/v1/builder/validators",
    ];

    let helix_register_endpoint = "http://localhost:4040/eth/v1/builder/validators";
    let beacon_client = helix_beacon_client::beacon_client::BeaconClient::from_endpoint_str("http://localhost:5052");

    let (head_event_sender, mut head_event_receiver) = tokio::sync::broadcast::channel::<helix_beacon_client::types::HeadEventData>(100);

    tokio::spawn(async move {
        if let Err(err) = beacon_client.subscribe_to_head_events(head_event_sender).await {
            println!("Error subscribing to head events: {err}");
        }
    });

    let mut first_fetch_complete = false;
    // Process registrations each half epoch
    while let Ok(head_event) = head_event_receiver.recv().await {
        println!("New head event: {}", head_event.slot);
        if head_event.slot % 5 != 0 && first_fetch_complete {
            continue;
        }
        first_fetch_complete = true;

        println!("Replaying validator registrations");

        match fetch_and_aggregate_validators(&endpoints).await {
            Ok(validators) => {
                println!("Num validators fetched: {:?}", validators.len());
                if let Err(err) = register_validators(validators.into_iter().map(|v| v.registration).collect(), helix_register_endpoint).await {
                    println!("Error registering validators to our relay: {err}");
                } else {
                    println!("Success!");
                }
            }
            Err(err) => {
                println!("Error fetching validators: {err}");
            }
        }
    }
}
