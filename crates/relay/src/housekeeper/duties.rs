use std::sync::Arc;

use alloy_primitives::B256;
use helix_common::{
    ProposerDuty, SignedValidatorRegistrationEntry,
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    beacon::{MultiBeaconClient, types::BeaconResponse},
    http::client::{HttpClient, PendingResponse},
    is_local_dev,
    local_cache::LocalCache,
    validator_preferences::ValidatorPreferences,
};
use helix_database::handle::DbHandle;
use helix_types::{BlsPublicKeyBytes, SignedValidatorRegistration, Slot, ValidatorRegistration};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{error, info};
use url::Url;

// State machine for the two-epoch proposer duties fetch (current + next epoch).
pub enum DutiesFetchState {
    Current { req: PendingResponse, next_epoch_url: Url },
    Next { current: Vec<ProposerDuty>, dependent_root: Option<B256>, req: PendingResponse },
    Done,
}

pub struct DutiesUpdate {
    pub duties: Vec<ProposerDuty>,
    /// A change means a reorg moved the block the duties were computed from.
    pub dependent_root: Option<B256>,
    /// False when the next-epoch request failed, so `duties` covers one epoch only.
    pub has_next_epoch: bool,
}

fn dependent_root(meta: &FxHashMap<String, serde_json::Value>) -> Option<B256> {
    meta.get("dependent_root")?.as_str()?.parse().ok()
}

/// Keep the duties a partial fetch did not cover, so one failed request does not shrink
/// the lookahead the relay gives builders.
pub fn merge_partial_duties(
    update: DutiesUpdate,
    known: &[ProposerDuty],
    first_uncovered_slot: Slot,
) -> Vec<ProposerDuty> {
    if update.has_next_epoch {
        return update.duties;
    }
    let mut duties = update.duties;
    let covered: FxHashSet<Slot> = duties.iter().map(|d| d.slot).collect();
    duties.extend(
        known
            .iter()
            .filter(|d| d.slot >= first_uncovered_slot && !covered.contains(&d.slot))
            .cloned(),
    );
    duties.sort_by_key(|d| d.slot);
    duties
}

/// Keeps a missing duty pubkey to one fetch per epoch, not one per slot.
#[derive(Default)]
pub struct RequestedRegistrations {
    epoch: u64,
    keys: FxHashSet<BlsPublicKeyBytes>,
}

impl RequestedRegistrations {
    pub fn take_new(
        &mut self,
        epoch: u64,
        keys: impl Iterator<Item = BlsPublicKeyBytes>,
    ) -> Vec<BlsPublicKeyBytes> {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.keys.clear();
        }
        keys.filter(|key| self.keys.insert(*key)).collect()
    }
}

impl DutiesFetchState {
    pub fn new(
        http_client: &HttpClient,
        beacon_client: &MultiBeaconClient,
        epoch: u64,
        attempt: usize,
    ) -> Option<Self> {
        let c = beacon_client.beacon_clients_by_last_response().nth(attempt)?;
        let url = c.config.url.join(&format!("/eth/v1/validator/duties/proposer/{epoch}")).ok()?;
        let next_url =
            c.config.url.join(&format!("/eth/v1/validator/duties/proposer/{}", epoch + 1)).ok()?;
        let req = http_client.get(&url).ok()?;
        Some(Self::Current { req, next_epoch_url: next_url })
    }

    pub fn poll(
        &mut self,
        http_client: &HttpClient,
    ) -> std::task::Poll<Result<DutiesUpdate, Box<dyn std::error::Error>>> {
        use std::task::Poll;
        loop {
            match std::mem::replace(self, Self::Done) {
                Self::Current { mut req, next_epoch_url } => {
                    match req.poll_json::<BeaconResponse<Vec<ProposerDuty>>>() {
                        Poll::Pending => {
                            *self = Self::Current { req, next_epoch_url };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                        Poll::Ready(Ok(resp)) => {
                            let root = dependent_root(&resp.meta);
                            match http_client.get(&next_epoch_url) {
                                Err(e) => {
                                    error!(%e, epoch_offset = 1, "failed to start next duties fetch");
                                    return Poll::Ready(Ok(DutiesUpdate {
                                        duties: resp.data,
                                        dependent_root: root,
                                        has_next_epoch: false,
                                    }));
                                }
                                Ok(next_req) => {
                                    *self = Self::Next {
                                        current: resp.data,
                                        dependent_root: root,
                                        req: next_req,
                                    };
                                }
                            }
                        }
                    }
                }
                Self::Next { mut current, dependent_root, mut req } => {
                    match req.poll_json::<BeaconResponse<Vec<ProposerDuty>>>() {
                        Poll::Pending => {
                            *self = Self::Next { current, dependent_root, req };
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(e)) => {
                            error!(%e, epoch_offset = 1, "failed fetching next epoch duties");
                            return Poll::Ready(Ok(DutiesUpdate {
                                duties: current,
                                dependent_root,
                                has_next_epoch: false,
                            }));
                        }
                        Poll::Ready(Ok(mut resp)) => {
                            current.append(&mut resp.data);
                            return Poll::Ready(Ok(DutiesUpdate {
                                duties: current,
                                dependent_root,
                                has_next_epoch: true,
                            }));
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
    registrations: &FxHashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry>,
) -> Vec<BuilderGetValidatorsResponseEntry> {
    proposer_duties
        .iter()
        .filter_map(|duty| {
            let entry = match registrations.get(&duty.pubkey) {
                Some(reg) => reg.registration_info.clone(),
                None if is_local_dev() => local_dev_registration_info(duty),
                None => return None,
            };
            Some(BuilderGetValidatorsResponseEntry {
                slot: duty.slot,
                validator_index: duty.validator_index,
                entry,
            })
        })
        .collect()
}

/// Local dev runs against a live beacon chain with no real registrations, so
/// every duty gets an unsigned stand-in; validation skips the fee-recipient
/// check for these (`is_local_dev` in `validate_submission_data`).
fn local_dev_registration_info(duty: &ProposerDuty) -> ValidatorRegistrationInfo {
    ValidatorRegistrationInfo {
        registration: SignedValidatorRegistration {
            message: ValidatorRegistration {
                fee_recipient: Default::default(),
                gas_limit: 60_000_000,
                timestamp: 0,
                pubkey: duty.pubkey,
            },
            signature: Default::default(),
        },
        preferences: ValidatorPreferences::default(),
    }
}

pub fn process_duties(
    proposer_duties: &[ProposerDuty],
    local_cache: &Arc<LocalCache>,
    db: &DbHandle,
    requested: &mut RequestedRegistrations,
    epoch: u64,
) {
    let pubkeys: Vec<BlsPublicKeyBytes> = proposer_duties.iter().map(|d| d.pubkey).collect();
    let registrations: Vec<SignedValidatorRegistrationEntry> =
        local_cache.get_validator_registrations_for_pub_keys(&pubkeys);
    let registrations: FxHashMap<BlsPublicKeyBytes, SignedValidatorRegistrationEntry> =
        registrations.into_iter().map(|e| (*e.public_key(), e)).collect();

    let missing = requested
        .take_new(epoch, pubkeys.iter().copied().filter(|key| !registrations.contains_key(key)));
    if !missing.is_empty() {
        info!(missing = missing.len(), "fetching registrations missing for upcoming duties");
        db.fetch_validator_registrations(missing);
    }

    let formatted: Vec<BuilderGetValidatorsResponseEntry> =
        _build_formatted_duties(proposer_duties, &registrations);

    info!(duties = formatted.len(), registered = formatted.len(), "storing proposer duties");
    local_cache.update_proposer_duties(formatted.clone());
    db.set_proposer_duties(formatted);
}

#[cfg(test)]
mod tests {
    use helix_common::validator_preferences::ValidatorPreferences;
    use helix_database::{DbRequest, PendingBlockSubmissionValue};
    use helix_types::Slot;

    use super::*;

    type Requests = crossbeam_channel::Receiver<DbRequest>;

    fn pubkey(byte: u8) -> BlsPublicKeyBytes {
        BlsPublicKeyBytes::repeat_byte(byte)
    }

    fn duty(slot: u64, pubkey: BlsPublicKeyBytes) -> ProposerDuty {
        ProposerDuty { pubkey, validator_index: slot, slot: Slot::new(slot) }
    }

    fn harness() -> (Arc<LocalCache>, DbHandle, Requests) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (batch_sender, _batch_receiver) =
            crossbeam_channel::unbounded::<PendingBlockSubmissionValue>();
        (Arc::new(LocalCache::new()), DbHandle::new(sender, batch_sender), receiver)
    }

    fn register(cache: &LocalCache, pubkey: BlsPublicKeyBytes) {
        let info = ValidatorRegistrationInfo {
            registration: SignedValidatorRegistration {
                message: ValidatorRegistration {
                    fee_recipient: Default::default(),
                    gas_limit: 60_000_000,
                    timestamp: 0,
                    pubkey,
                },
                signature: Default::default(),
            },
            preferences: ValidatorPreferences::default(),
        };
        cache.save_validator_registrations(std::iter::once(info), None);
    }

    fn fetched(receiver: &Requests) -> Vec<BlsPublicKeyBytes> {
        let mut keys = Vec::new();
        while let Ok(request) = receiver.try_recv() {
            if let DbRequest::FetchValidatorRegistrations { pub_keys } = request {
                keys.extend(pub_keys);
            }
        }
        keys.sort();
        keys
    }

    #[test]
    fn fetches_only_the_duty_pubkeys_missing_from_the_cache() {
        let (cache, db, receiver) = harness();
        register(&cache, pubkey(1));
        let duties = vec![duty(1, pubkey(1)), duty(2, pubkey(2)), duty(3, pubkey(3))];

        process_duties(&duties, &cache, &db, &mut RequestedRegistrations::default(), 0);

        assert_eq!(fetched(&receiver), vec![pubkey(2), pubkey(3)]);
    }

    #[test]
    fn fetches_nothing_when_every_duty_is_registered() {
        let (cache, db, receiver) = harness();
        register(&cache, pubkey(1));
        let duties = vec![duty(1, pubkey(1))];

        process_duties(&duties, &cache, &db, &mut RequestedRegistrations::default(), 0);

        assert!(fetched(&receiver).is_empty());
    }

    #[test]
    fn requests_a_missing_pubkey_once_per_epoch() {
        let (cache, db, receiver) = harness();
        let duties = vec![duty(1, pubkey(2))];
        let mut requested = RequestedRegistrations::default();

        process_duties(&duties, &cache, &db, &mut requested, 7);
        assert_eq!(fetched(&receiver), vec![pubkey(2)]);

        process_duties(&duties, &cache, &db, &mut requested, 7);
        assert!(fetched(&receiver).is_empty());

        process_duties(&duties, &cache, &db, &mut requested, 8);
        assert_eq!(fetched(&receiver), vec![pubkey(2)]);
    }

    #[test]
    fn requests_a_pubkey_once_when_it_has_two_duties_in_the_window() {
        let (cache, db, receiver) = harness();
        let duties = vec![duty(1, pubkey(2)), duty(40, pubkey(2))];

        process_duties(&duties, &cache, &db, &mut RequestedRegistrations::default(), 0);

        assert_eq!(fetched(&receiver), vec![pubkey(2)]);
    }

    fn update(duties: Vec<ProposerDuty>, has_next_epoch: bool) -> DutiesUpdate {
        DutiesUpdate { duties, dependent_root: None, has_next_epoch }
    }

    #[test]
    fn a_complete_fetch_replaces_the_duty_list() {
        let known = vec![duty(40, pubkey(9))];

        let merged =
            merge_partial_duties(update(vec![duty(1, pubkey(1))], true), &known, Slot::new(32));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].slot, Slot::new(1));
    }

    #[test]
    fn a_partial_fetch_keeps_the_duties_it_did_not_cover() {
        let known = vec![duty(1, pubkey(1)), duty(40, pubkey(9))];

        let merged =
            merge_partial_duties(update(vec![duty(1, pubkey(1))], false), &known, Slot::new(32));

        let slots: Vec<u64> = merged.iter().map(|d| d.slot.as_u64()).collect();
        assert_eq!(slots, vec![1, 40]);
    }

    #[test]
    fn a_partial_fetch_prefers_the_fresh_duty_for_a_slot_it_covered() {
        let known = vec![duty(1, pubkey(9))];

        let merged =
            merge_partial_duties(update(vec![duty(1, pubkey(1))], false), &known, Slot::new(0));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].pubkey, pubkey(1));
    }

    #[test]
    fn builds_the_duties_it_can_when_a_registration_is_missing() {
        let (cache, db, _receiver) = harness();
        register(&cache, pubkey(1));
        let duties = vec![duty(1, pubkey(1)), duty(2, pubkey(2))];

        process_duties(&duties, &cache, &db, &mut RequestedRegistrations::default(), 0);

        let stored = cache.get_proposer_duties();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].entry.registration.message.pubkey, pubkey(1));
    }
}
