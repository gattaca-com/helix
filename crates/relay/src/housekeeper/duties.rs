use std::{collections::HashMap, sync::Arc};

use helix_common::{
    ProposerDuty, SignedValidatorRegistrationEntry,
    api::builder_api::BuilderGetValidatorsResponseEntry,
    beacon::{BeaconClientError, MultiBeaconClient, types::BeaconResponse},
    http::client::{HttpClient, PendingResponse},
    local_cache::LocalCache,
};
use helix_database::handle::DbHandle;
use helix_types::BlsPublicKeyBytes;
use tracing::{error, info};
use url::Url;

// State machine for the two-epoch proposer duties fetch (current + next epoch).
pub enum DutiesFetchState {
    Current { req: PendingResponse, next_epoch_url: Url },
    Next { current: Vec<ProposerDuty>, req: PendingResponse },
    Done,
}

impl DutiesFetchState {
    pub fn new(
        http_client: &HttpClient,
        beacon_client: &MultiBeaconClient,
        epoch: u64,
    ) -> Option<Self> {
        let c = beacon_client.beacon_clients_by_last_response().next()?;
        let url = c.config.url.join(&format!("/eth/v1/validator/duties/proposer/{epoch}")).ok()?;
        let next_url =
            c.config.url.join(&format!("/eth/v1/validator/duties/proposer/{}", epoch + 1)).ok()?;
        let req = http_client.get(&url).ok()?;
        Some(Self::Current { req, next_epoch_url: next_url })
    }

    pub fn poll(
        &mut self,
        http_client: &HttpClient,
    ) -> std::task::Poll<Result<Vec<ProposerDuty>, Box<dyn std::error::Error>>> {
        use std::task::Poll;
        loop {
            match std::mem::replace(self, Self::Done) {
                Self::Current { mut req, next_epoch_url } => {
                    match req.poll_json::<BeaconResponse<Vec<ProposerDuty>>>() {
                        Poll::Pending => {
                            *self = Self::Current { req, next_epoch_url };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(resp)) => match http_client.get(&next_epoch_url) {
                            Err(e) => {
                                error!(%e, epoch_offset = 1, "failed to start next duties fetch");
                                return Poll::Ready(Ok(resp.data));
                            }
                            Ok(next_req) => {
                                *self = Self::Next { current: resp.data, req: next_req };
                            }
                        },
                    }
                }
                Self::Next { mut current, mut req } => {
                    match req.poll_json::<BeaconResponse<Vec<ProposerDuty>>>() {
                        Poll::Pending => {
                            *self = Self::Next { current, req };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => {
                            error!(%e, epoch_offset = 1, "failed fetching next epoch duties");
                            return Poll::Ready(Ok(current));
                        }
                        Poll::Ready(Ok(mut resp)) => {
                            current.append(&mut resp.data);
                            return Poll::Ready(Ok(current));
                        }
                    }
                }
                Self::Done => panic!("DutiesFetchState::poll after completion"),
            }
        }
    }
}

fn _build_formatted_duties(
    proposer_duties: &[ProposerDuty],
    registrations: &HashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>,
) -> Vec<BuilderGetValidatorsResponseEntry> {
    proposer_duties
        .iter()
        .filter_map(|duty| {
            registrations.get(&duty.pubkey).map(|reg| BuilderGetValidatorsResponseEntry {
                slot: duty.slot,
                validator_index: duty.validator_index,
                entry: reg.registration_info.clone(),
            })
        })
        .collect()
}

pub fn process_duties(
    proposer_duties: &[ProposerDuty],
    local_cache: &Arc<LocalCache>,
    db: &DbHandle,
) {
    let pubkeys: Vec<BlsPublicKeyBytes> = proposer_duties.iter().map(|d| d.pubkey).collect();
    let registrations: Vec<SignedValidatorRegistrationEntry> =
        local_cache.get_validator_registrations_for_pub_keys(&pubkeys);
    let registrations = registrations.into_iter().map(|e| (*e.public_key(), e)).collect();
    let formatted: Vec<BuilderGetValidatorsResponseEntry> =
        _build_formatted_duties(proposer_duties, &registrations);

    info!(duties = formatted.capacity(), registered = formatted.len(), "storing proposer duties");
    local_cache.update_proposer_duties(formatted.clone());
    db.set_proposer_duties(formatted);
}

pub async fn fetch_duties(
    epoch: u64,
    beacon_client: Arc<MultiBeaconClient>,
) -> Result<Vec<ProposerDuty>, BeaconClientError> {
    let (_, mut proposer_duties) = beacon_client.get_proposer_duties(epoch).await?;
    match beacon_client.get_proposer_duties(epoch + 1).await {
        Ok((_, mut next_duties)) => proposer_duties.append(&mut next_duties),
        Err(err) => error!(epoch = epoch + 1, %err, "failed fetching next proposer duties"),
    }
    Ok(proposer_duties)
}
