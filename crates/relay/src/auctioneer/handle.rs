use std::sync::Arc;

use dashmap::DashMap;
use futures::{FutureExt, future::Shared};
use helix_common::{
    GetPayloadTrace,
    api::proposer_api::{GetExecutionPayloadBidParams, GetHeaderParams},
    chain_info::ChainInfo,
    utils::utcnow_ns,
};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, ExecPayload, GetPayloadResponse, SigError,
    SignedBlindedBeaconBlock,
};
use tokio::sync::oneshot::{self, Receiver};
use tracing::trace;

use crate::{
    api::proposer::{ProposerApiError, get_payload::ProposerApiVersion},
    auctioneer::types::{
        Event, GetExecutionPayloadBidResult, GetHeaderResult, GetPayloadResult,
        SubmitBuilderPreferencesResult,
    },
    gossip::BroadcastPayloadParams,
};

/// 96-byte BLS signature used as dedup key for get_payload.
type SignatureKey = [u8; 96];

/// Dedup waiters only get the response on success or a flag on error.
pub type DedupPayloadResult = Option<GetPayloadResponse>;
type SharedPayloadFut = Shared<Receiver<Arc<DedupPayloadResult>>>;
//Shared<Pin<Box<dyn Future<Output = Arc<DedupPayloadResult>> + Send + 'static>>>;

pub enum GetPayloadKind {
    /// First caller — owns the oneshot, responsible for full processing.
    /// Must send the final response through `dedup_tx` to resolve dedup waiters.
    Primary {
        rx: oneshot::Receiver<GetPayloadResult>,
        dedup_tx: oneshot::Sender<Arc<DedupPayloadResult>>,
    },
    /// Duplicate caller — shared result, only returns the response.
    Dedup(SharedPayloadFut),
}

#[derive(Clone)]
pub struct AuctioneerHandle {
    auctioneer: crossbeam_channel::Sender<Event>,
    /// Dedup concurrent get_payload calls with the same block signature.
    inflight_payloads: Arc<DashMap<(ProposerApiVersion, SignatureKey), SharedPayloadFut>>,
}

impl AuctioneerHandle {
    pub fn new(auctioneer: crossbeam_channel::Sender<Event>) -> Self {
        Self { auctioneer, inflight_payloads: Arc::new(DashMap::new()) }
    }

    pub fn get_header(
        &self,
        params: GetHeaderParams,
        is_mev_boost: bool,
    ) -> Result<oneshot::Receiver<GetHeaderResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        trace!("sending to auctioneer");
        self.auctioneer
            .try_send(Event::GetHeader {
                params,
                res_tx: tx,
                span: tracing::Span::current(),
                is_mev_boost,
            })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn get_execution_payload_bid(
        &self,
        params: GetExecutionPayloadBidParams,
    ) -> Result<oneshot::Receiver<GetExecutionPayloadBidResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        trace!("sending to auctioneer");
        self.auctioneer
            .try_send(Event::GetExecutionPayloadBid {
                params,
                res_tx: tx,
                span: tracing::Span::current(),
            })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn submit_builder_preferences(
        &self,
        proposer_pubkey: BlsPublicKeyBytes,
        slot: u64,
        max_execution_payment: u64,
    ) -> Result<oneshot::Receiver<SubmitBuilderPreferencesResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        trace!("sending to auctioneer");
        self.auctioneer
            .try_send(Event::SubmitBuilderPreferences {
                proposer_pubkey,
                slot,
                max_execution_payment,
                res_tx: tx,
            })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn get_payload(
        &self,
        chain_info: &ChainInfo,
        api_version: ProposerApiVersion,
        proposer_pubkey: BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        mut trace: GetPayloadTrace,
    ) -> Result<GetPayloadKind, ChannelFull> {
        let key = (api_version, blinded_block.signature().serialize());
        if let Some(fut) = self.inflight_payloads.get(&key) {
            trace!("dedup get_payload hit");
            return Ok(GetPayloadKind::Dedup(fut.clone()));
        }

        // Verified before the entry is taken, so BLS verification never holds the shard lock
        // and a request that fails it doesn't claim the signature for the rest of the slot.
        let (res_tx, rx) = oneshot::channel();
        let block_hash =
            verify_signed_blinded_block_signature(chain_info, &blinded_block, &proposer_pubkey)
                .map_err(ProposerApiError::from)
                .and_then(|()| {
                    blinded_block
                        .message()
                        .body()
                        .execution_payload()
                        .map_err(|_| ProposerApiError::InvalidFork)
                        .map(|p| p.block_hash().0)
                });
        let block_hash = match block_hash {
            Ok(block_hash) => block_hash,
            Err(err) => {
                let _ = res_tx.send(Err(err));
                // Nothing waits on a request that was never registered for dedup.
                let (dedup_tx, _) = oneshot::channel();
                return Ok(GetPayloadKind::Primary { rx, dedup_tx });
            }
        };
        trace.signature_validated = utcnow_ns();

        match self.inflight_payloads.entry(key) {
            // A duplicate verified concurrently and registered first.
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                trace!("dedup get_payload hit");
                Ok(GetPayloadKind::Dedup(entry.get().clone()))
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                tracing::trace!("sending to auctioneer");
                if self
                    .auctioneer
                    .try_send(Event::GetPayload {
                        block_hash,
                        blinded: Box::new(blinded_block),
                        trace,
                        res_tx,
                        span: tracing::Span::current(),
                    })
                    .is_err()
                {
                    tracing::error!("failed to send get_payload to auctioneer");
                    return Err(ChannelFull);
                }

                // Shared future for dedup callers. Primary caller completes it via dedup_tx.
                let (dedup_tx, dedup_rx) = oneshot::channel::<Arc<DedupPayloadResult>>();
                entry.insert(dedup_rx.shared());
                Ok(GetPayloadKind::Primary { rx, dedup_tx })
            }
        }
    }

    pub fn clear_inflight_payloads(&self) {
        self.inflight_payloads.clear();
    }

    pub fn gossip_payload(&self, req: BroadcastPayloadParams) -> Result<(), ChannelFull> {
        trace!("sending to worker");
        self.auctioneer.try_send(Event::GossipPayload(req)).map_err(|_| ChannelFull)
    }

    pub fn send_event(&self, event: Event) -> Result<(), ChannelFull> {
        trace!("sending event to auctioneer: {:?}", event.as_str());
        self.auctioneer.try_send(event).map_err(|_| ChannelFull)
    }
}

pub struct ChannelFull;

fn verify_signed_blinded_block_signature(
    chain_info: &ChainInfo,
    signed_blinded_beacon_block: &SignedBlindedBeaconBlock,
    public_key: &BlsPublicKeyBytes,
) -> Result<(), SigError> {
    let uncompressed_public_key = BlsPublicKey::deserialize(public_key.as_slice())
        .map_err(|_| SigError::InvalidBlsPubkeyBytes)?;
    let slot = signed_blinded_beacon_block.message().slot();
    let epoch = slot.epoch(chain_info.slots_per_epoch());
    let fork = chain_info.spec.fork_at_epoch(epoch);

    let valid = signed_blinded_beacon_block.verify_signature(
        None,
        &uncompressed_public_key,
        &fork,
        chain_info.genesis_validators_root,
        &chain_info.spec,
    );

    if !valid {
        return Err(SigError::InvalidBlsSignature);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use helix_common::utils::install_default_crypto_provider;
    use helix_types::{
        BlsKeypair, BlsSignature, Domain, EmptyBlock, SignedBlindedBeaconBlockFulu, SigningData,
    };
    use tree_hash::TreeHash;

    use super::*;

    /// A Fulu blinded block signed by `signer`.
    fn signed_block(chain_info: &ChainInfo, signer: &BlsKeypair) -> SignedBlindedBeaconBlock {
        let spec = &chain_info.spec;
        let mut block = SignedBlindedBeaconBlockFulu {
            message: EmptyBlock::empty(spec),
            signature: BlsSignature::empty(),
        };
        block.message.slot = spec.fulu_fork_epoch.unwrap().start_slot(chain_info.slots_per_epoch());
        let domain = chain_info.domain_at(Domain::BeaconProposer, block.message.slot);
        let object_root = block.message.tree_hash_root();
        block.signature = signer.sk.sign(SigningData { object_root, domain }.tree_hash_root());
        block.into()
    }

    fn pubkey(keypair: &BlsKeypair) -> BlsPublicKeyBytes {
        keypair.pk.compress().serialize().into()
    }

    fn request(
        handle: &AuctioneerHandle,
        chain_info: &ChainInfo,
        proposer: &BlsKeypair,
        block: &SignedBlindedBeaconBlock,
    ) -> GetPayloadKind {
        let Ok(kind) = handle.get_payload(
            chain_info,
            ProposerApiVersion::V1,
            pubkey(proposer),
            block.clone(),
            GetPayloadTrace::default(),
        ) else {
            panic!("auctioneer channel full");
        };
        kind
    }

    #[test]
    fn a_failed_signature_check_leaves_no_dedup_entry() {
        install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = AuctioneerHandle::new(tx);
        let proposer = BlsKeypair::random();
        let forged = signed_block(&chain_info, &BlsKeypair::random());

        for _ in 0..2 {
            let GetPayloadKind::Primary { mut rx, .. } =
                request(&handle, &chain_info, &proposer, &forged)
            else {
                panic!("a request that fails verification must not be deduplicated");
            };
            assert!(matches!(rx.try_recv(), Ok(Err(ProposerApiError::SigError(_)))));
        }
        assert!(handle.inflight_payloads.is_empty());
        assert!(rx.is_empty());
    }

    #[test]
    fn a_verified_duplicate_waits_on_the_primary() {
        install_default_crypto_provider();
        let chain_info = ChainInfo::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = AuctioneerHandle::new(tx);
        let proposer = BlsKeypair::random();
        let block = signed_block(&chain_info, &proposer);

        assert!(matches!(
            request(&handle, &chain_info, &proposer, &block),
            GetPayloadKind::Primary { .. }
        ));
        assert!(matches!(
            request(&handle, &chain_info, &proposer, &block),
            GetPayloadKind::Dedup(_)
        ));
        assert_eq!(rx.len(), 1);
    }
}
