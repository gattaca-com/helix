use std::{ops::Range, sync::Arc};

use dashmap::DashMap;
use futures::{FutureExt, future::Shared};
use helix_common::{GetPayloadTrace, api::proposer_api::GetHeaderParams, chain_info::ChainInfo};
use helix_types::{
    BlsPublicKey, BlsPublicKeyBytes, ExecPayload, GetPayloadResponse, SigError,
    SignedBlindedBeaconBlock, SignedValidatorRegistration,
};
use tokio::sync::oneshot::{self, Receiver};
use tracing::trace;

use crate::{
    api::proposer::{ProposerApiError, get_payload::ProposerApiVersion},
    auctioneer::types::{Event, GetHeaderResult, GetPayloadResult, RegWorkerJob},
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
    ) -> Result<oneshot::Receiver<GetHeaderResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        trace!("sending to auctioneer");
        self.auctioneer
            .try_send(Event::GetHeader { params, res_tx: tx, span: tracing::Span::current() })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn get_payload(
        &self,
        chain_info: &ChainInfo,
        api_version: ProposerApiVersion,
        proposer_pubkey: BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
    ) -> Result<GetPayloadKind, ChannelFull> {
        let sig_key: SignatureKey = blinded_block.signature().serialize();

        match self.inflight_payloads.entry((api_version, sig_key)) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                trace!("dedup get_payload hit");
                Ok(GetPayloadKind::Dedup(entry.get().clone()))
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let (res_tx, rx) = oneshot::channel();
                let blinded_block_hash: Result<_, ProposerApiError> = (|| {
                    verify_signed_blinded_block_signature(
                        chain_info,
                        &blinded_block,
                        &proposer_pubkey,
                    )?;
                    blinded_block
                        .message()
                        .body()
                        .execution_payload()
                        .map_err(|_| ProposerApiError::InvalidFork)
                        .map(|p| p.block_hash().0)
                })();

                match blinded_block_hash {
                    Ok(block_hash) => {
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
                            return Err(ChannelFull)
                        }
                    }
                    Err(err) => {
                        let _ = res_tx.send(Err(err));
                    }
                }

                // Shared future for dedup callers. Primary caller completes it via dedup_tx.
                let (dedup_tx, dedup_rx) = oneshot::channel::<Arc<DedupPayloadResult>>();
                let fut: SharedPayloadFut = dedup_rx.shared();
                entry.insert(fut);
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
}

pub struct ChannelFull;

#[derive(Clone)]
pub struct RegWorkerHandle {
    worker: crossbeam_channel::Sender<RegWorkerJob>,
}

impl RegWorkerHandle {
    pub fn new(worker: crossbeam_channel::Sender<RegWorkerJob>) -> Self {
        Self { worker }
    }

    pub fn send(
        &self,
        regs: Arc<Vec<SignedValidatorRegistration>>,
        range: Range<usize>,
    ) -> Result<oneshot::Receiver<Vec<(usize, bool)>>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.worker.try_send(RegWorkerJob { regs, range, res_tx: tx }).map_err(|_| ChannelFull)?;

        Ok(rx)
    }
}

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
