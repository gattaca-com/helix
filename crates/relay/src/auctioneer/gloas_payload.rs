use std::{
    net::SocketAddr,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use bytes::Bytes;
use helix_common::{
    Filtering, GetPayloadTrace,
    beacon::{MultiBeaconClient, beacon_client::BeaconClient, error::BeaconClientError},
    chain_info::ChainInfo,
    http::client::PendingResponse,
    utils::utcnow_ns,
};
use helix_database::types::SavePayloadParams;
use helix_types::{ForkName, GloasBidSubmission, PayloadAndBlobs, Slot, payload_from_v3};
use tracing::{error, info, warn};

use crate::auctioneer::{
    GloasBuilderIdentity, bid_adjustor::BidAdjustor, context::Context, seen_blocks::Recorded,
    types::SeenBlock,
};

const PUBLISH_ATTEMPTS: u32 = 12;
const PUBLISH_RETRY_INTERVAL: Duration = Duration::from_millis(100);

impl<B: BidAdjustor> Context<B> {
    pub(super) fn handle_gloas_block(&mut self, slot: Slot, seen: SeenBlock) {
        match self.seen_blocks.record(slot, seen) {
            Recorded::New => info!(%slot, ?seen, "seen a gloas block"),
            Recorded::Repeat => {}
            Recorded::Late => {
                warn!(%slot, ?seen, "gloas block arrived after the attestation deadline")
            }
        }
    }

    /// Reveals for `slot` only if exactly one block was seen, it names our bid, and it is
    /// our beacon node's head. Revealing earlier would hand an equivocating proposer a payload it
    /// can build a competing block on.
    pub(super) fn handle_attestation_deadline(&mut self, slot: Slot, head_root: B256) {
        let seen = self.seen_blocks.decide(slot);
        let ours = self.gloas_builder_identity.builder_index();
        let block = match seen.as_slice() {
            [] => return,
            [block] => *block,
            _ if seen.iter().any(|b| Some(b.bid_builder_index) == ours) => {
                error!(%slot, ?seen, "the proposer equivocated, withholding");
                return;
            }
            _ => {
                warn!(%slot, ?seen, "the proposer equivocated");
                return;
            }
        };
        let Some(builder_index) = ours else {
            return;
        };
        if block.bid_builder_index != builder_index {
            return;
        }
        if block.block_root != head_root {
            warn!(%slot, block_root = %block.block_root, %head_root, "block naming our bid is not our head, withholding");
            return;
        }
        let Some(entry) = self.gloas_payloads.get_for_reveal(&block.bid_block_hash) else {
            error!(%slot, bid_block_hash = %block.bid_block_hash, "block names our bid but we hold no payload");
            return;
        };

        let envelope = signed_envelope_contents(
            &entry.submission,
            builder_index,
            slot,
            block,
            &self.gloas_builder_identity,
            &self.chain_info,
        );
        let reveal =
            EnvelopeReveal::start(&self.beacon_client, &self.beacon_addrs, envelope.into(), slot);

        let submission = &entry.submission;
        let delivered = payload_from_v3(submission.payload_v3()).map(|execution_payload| {
            let trace = submission.bid_trace();
            let now = utcnow_ns();
            SavePayloadParams {
                slot: slot.as_u64(),
                builder_pub_key: trace.builder_pubkey,
                proposer_pub_key: trace.proposer_pubkey,
                value: trace.value,
                proposer_fee_recipient: entry.fee_recipient,
                payload: PayloadAndBlobs {
                    execution_payload: Arc::new(execution_payload),
                    blobs_bundle: Arc::new(submission.bundle()),
                },
                latency_trace: GetPayloadTrace {
                    receive: now,
                    on_deliver_payload: now,
                    ..Default::default()
                },
                user_agent: None,
                filtering: Filtering::default(),
            }
        });

        self.reveals.push(reveal);
        match delivered {
            Some(params) => self.db.save_delivered_payload(params),
            None => {
                error!(%slot, bid_block_hash = %block.bid_block_hash, "revealed payload does not convert for storage")
            }
        }
    }

    pub fn poll_reveals(&mut self) {
        let beacon_client = &self.beacon_client;
        let beacon_addrs = &self.beacon_addrs;
        self.reveals.retain_mut(|reveal| !reveal.poll(beacon_client, beacon_addrs));
    }
}

/// Builds and signs the `SignedExecutionPayloadEnvelope` fulfilling `block`'s bid.
fn signed_envelope_contents(
    submission: &GloasBidSubmission,
    builder_index: u64,
    slot: Slot,
    block: SeenBlock,
    identity: &GloasBuilderIdentity,
    chain_info: &ChainInfo,
) -> Vec<u8> {
    let envelope = submission.envelope(builder_index, block.block_root, block.parent_root);
    let signed_envelope = identity.sign_envelope(&envelope, slot.as_u64(), chain_info);
    submission.envelope_contents(&signed_envelope)
}

/// Offers the envelope to the beacon nodes until one takes it. A node rejects it
/// until it has imported the block it names.
pub struct EnvelopeReveal {
    envelope: Bytes,
    slot: Slot,
    attempt: u32,
    /// The requests of the attempt that is out, one per beacon node.
    pending: Vec<PendingResponse>,
    taken: bool,
    last_error: Option<BeaconClientError>,
    /// Set between attempts.
    retry_at: Option<Instant>,
}

impl EnvelopeReveal {
    fn start(
        beacon_client: &MultiBeaconClient,
        beacon_addrs: &[Option<SocketAddr>],
        envelope: Bytes,
        slot: Slot,
    ) -> Self {
        let mut reveal = Self {
            envelope,
            slot,
            attempt: 0,
            pending: Vec::with_capacity(beacon_addrs.len()),
            taken: false,
            last_error: None,
            retry_at: None,
        };
        reveal.send(beacon_client, beacon_addrs);
        reveal
    }

    fn send(&mut self, beacon_client: &MultiBeaconClient, beacon_addrs: &[Option<SocketAddr>]) {
        for (client, addr) in beacon_client.beacon_clients.iter().zip(beacon_addrs) {
            let Some(addr) = *addr else { continue };
            match client.send_execution_payload_envelope(
                addr,
                self.envelope.clone(),
                ForkName::Gloas,
            ) {
                Ok(pending) => self.pending.push(pending),
                Err(err) => self.last_error = Some(err),
            }
        }
    }

    /// Returns whether the reveal is over, either way.
    fn poll(
        &mut self,
        beacon_client: &MultiBeaconClient,
        beacon_addrs: &[Option<SocketAddr>],
    ) -> bool {
        if let Some(at) = self.retry_at {
            if Instant::now() < at {
                return false;
            }
            self.retry_at = None;
            self.send(beacon_client, beacon_addrs);
        }

        let taken = &mut self.taken;
        let last_error = &mut self.last_error;
        self.pending.retain_mut(|pending| {
            let Poll::Ready(res) = pending.poll_bytes() else { return true };
            let res = res
                .map_err(BeaconClientError::from)
                .and_then(|(status, body)| BeaconClient::envelope_outcome(status, &body));
            match res {
                Ok(_) => *taken = true,
                Err(err) => *last_error = Some(err),
            }
            false
        });
        if !self.pending.is_empty() {
            return false;
        }

        let slot = self.slot;
        let attempt = self.attempt;
        if self.taken {
            info!(%slot, attempt, "revealed the payload");
            return true;
        }
        let err = self.last_error.take().unwrap_or(BeaconClientError::BeaconNodeUnavailable);
        warn!(%err, attempt, "could not reveal the payload yet");
        self.attempt += 1;
        if self.attempt == PUBLISH_ATTEMPTS {
            error!(%slot, "gave up revealing the payload after {PUBLISH_ATTEMPTS} attempts");
            return true;
        }
        self.retry_at = Some(Instant::now() + PUBLISH_RETRY_INTERVAL);
        false
    }
}

#[cfg(test)]
mod tests {
    use helix_common::{
        BeaconClientConfig, http::client::HttpClient, utils::install_default_crypto_provider,
    };
    use helix_types::{
        BeaconBlockGloas, BidTrace, BlsKeypair, EmptyBlock, EthSpec, ExecutionBlockHash,
        ExecutionPayloadGloas, GloasBidTrace, MainnetEthSpec, SignedBeaconBlockGloas,
        SignedBidSubmissionGloas, SignedExecutionPayloadEnvelope, TestRandomSeed,
        gloas::GloasBlock,
    };
    use httpmock::{Method::POST, Mock, MockServer};
    use ssz::{Decode, Encode};
    use tree_hash::TreeHash;

    use super::*;

    fn submission(block_hash: B256) -> GloasBidSubmission {
        let mut execution_payload = ExecutionPayloadGloas::default();
        execution_payload.block_hash = ExecutionBlockHash(block_hash);
        let mut trace = BidTrace::test_random();
        trace.block_hash = block_hash;
        let submission = SignedBidSubmissionGloas {
            message: GloasBidTrace { trace, parent_block_root: B256::ZERO },
            execution_payload: Arc::new(execution_payload),
            blobs_bundle: Default::default(),
            execution_requests: Default::default(),
            signature: Default::default(),
        };
        GloasBidSubmission::from_ssz_bytes(submission.as_ssz_bytes().into()).unwrap()
    }

    fn block(block_hash: B256, builder_index: u64, parent_root: B256) -> Vec<u8> {
        let mut message = BeaconBlockGloas::empty(&ChainInfo::default().spec);
        message.parent_root = parent_root;
        message.body.signed_execution_payload_bid.message.block_hash =
            ExecutionBlockHash(block_hash);
        message.body.signed_execution_payload_bid.message.builder_index = builder_index;
        SignedBeaconBlockGloas { message, signature: helix_types::BlsSignature::empty() }
            .as_ssz_bytes()
    }

    fn identity() -> GloasBuilderIdentity {
        install_default_crypto_provider();
        GloasBuilderIdentity::new(BlsKeypair::random())
    }

    fn construct(block: &[u8], identity: &GloasBuilderIdentity) -> SignedExecutionPayloadEnvelope {
        let view = GloasBlock::new(block).expect("a well-formed block");
        let seen = SeenBlock::new(&view, view.block_root());
        let contents = signed_envelope_contents(
            &submission(seen.bid_block_hash),
            view.bid_builder_index(),
            Slot::new(view.slot()),
            seen,
            identity,
            &ChainInfo::default(),
        );
        let proofs_offset = u32::from_le_bytes(contents[4..8].try_into().unwrap()) as usize;
        SignedExecutionPayloadEnvelope::from_ssz_bytes(&contents[12..proofs_offset])
            .expect("the envelope decodes as the consensus type")
    }

    #[test]
    fn the_envelope_matches_the_block_and_payload() {
        let block_hash = B256::repeat_byte(0x11);
        let parent_root = B256::repeat_byte(0x22);
        let block = block(block_hash, 7, parent_root);

        let signed_envelope = construct(&block, &identity());

        let message = &signed_envelope.message;
        assert_eq!(message.builder_index, 7);
        assert_eq!(
            message.beacon_block_root,
            SignedBeaconBlockGloas::from_ssz_bytes(&block).unwrap().message.tree_hash_root()
        );
        assert_eq!(message.parent_beacon_block_root, parent_root);
        assert_eq!(message.payload.block_hash.0, block_hash);
    }

    #[test]
    fn the_envelope_signature_verifies_against_our_identity() {
        let chain_info = ChainInfo::default();
        let identity = identity();

        let signed_envelope = construct(&block(B256::repeat_byte(0x33), 3, B256::ZERO), &identity);

        let epoch = signed_envelope.message.slot().epoch(MainnetEthSpec::slots_per_epoch());
        let fork = chain_info.spec.fork_at_epoch(epoch);
        assert!(signed_envelope.verify_signature(
            &identity.keypair.pk,
            &fork,
            chain_info.genesis_validators_root,
            &chain_info.spec,
        ));
    }

    const ENVELOPES: &str = "/eth/v1/beacon/execution_payload_envelopes";

    fn answering(server: &MockServer, status: u16) -> Mock<'_> {
        server.mock(|when, then| {
            when.method(POST).path(ENVELOPES);
            then.status(status).json_body(serde_json::json!({ "code": status, "message": "" }));
        })
    }

    fn nodes(servers: &[&MockServer]) -> (MultiBeaconClient, Vec<Option<SocketAddr>>) {
        install_default_crypto_provider();
        let urls = servers.iter().map(|server| server.url("/").parse().unwrap());
        let clients: Vec<_> =
            urls.map(|url| Arc::new(BeaconClient::new(BeaconClientConfig { url }))).collect();
        let addrs =
            clients.iter().map(|client| HttpClient::resolve(&client.config.url).ok()).collect();
        (MultiBeaconClient::new(clients), addrs)
    }

    fn start(client: &MultiBeaconClient, addrs: &[Option<SocketAddr>]) -> EnvelopeReveal {
        EnvelopeReveal::start(client, addrs, Bytes::from_static(&[0u8; 12]), Slot::new(1))
    }

    #[test]
    fn one_node_taking_the_envelope_ends_the_reveal() {
        let (refusing, taking) = (MockServer::start(), MockServer::start());
        answering(&refusing, 500);
        answering(&taking, 200);
        let (client, addrs) = nodes(&[&refusing, &taking]);

        let mut reveal = start(&client, &addrs);
        while !reveal.poll(&client, &addrs) {}

        assert!(reveal.taken);
        assert_eq!(reveal.attempt, 0);
    }

    #[test]
    fn a_refused_envelope_is_offered_again() {
        let server = MockServer::start();
        let mut refusal = answering(&server, 500);
        let (client, addrs) = nodes(&[&server]);

        let mut reveal = start(&client, &addrs);
        while reveal.attempt == 0 {
            assert!(!reveal.poll(&client, &addrs));
        }
        refusal.delete();
        answering(&server, 200);
        while !reveal.poll(&client, &addrs) {}

        assert!(reveal.taken);
        assert_eq!(reveal.attempt, 1);
    }

    #[test]
    fn the_reveal_gives_up_after_the_last_attempt() {
        let server = MockServer::start();
        answering(&server, 500);
        let (client, addrs) = nodes(&[&server]);

        let mut reveal = start(&client, &addrs);
        while !reveal.poll(&client, &addrs) {}

        assert!(!reveal.taken);
        assert_eq!(reveal.attempt, PUBLISH_ATTEMPTS);
    }
}
