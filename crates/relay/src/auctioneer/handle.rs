use std::{ops::Range, sync::Arc};

use dashmap::DashMap;
use futures::{FutureExt, future::Shared};
use helix_common::{GetPayloadTrace, api::proposer_api::GetHeaderParams};
use helix_types::{
    BlsPublicKeyBytes, GetPayloadResponse, SignedBlindedBeaconBlock, SignedValidatorRegistration,
};
use tokio::sync::oneshot::{self, Receiver};
use tracing::trace;

use crate::{
    auctioneer::types::{Event, GetHeaderResult, GetPayload, GetPayloadResult, RegWorkerJob},
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
    worker: crossbeam_channel::Sender<GetPayload>,
    auctioneer: crossbeam_channel::Sender<Event>,
    /// Dedup concurrent get_payload calls with the same block signature.
    inflight_payloads: Arc<DashMap<SignatureKey, SharedPayloadFut>>,
}

impl AuctioneerHandle {
    pub fn new(
        worker: crossbeam_channel::Sender<GetPayload>,
        auctioneer: crossbeam_channel::Sender<Event>,
    ) -> Self {
        Self { worker, auctioneer, inflight_payloads: Arc::new(DashMap::new()) }
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
        proposer_pubkey: BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
    ) -> Result<GetPayloadKind, ChannelFull> {
        let sig_key: SignatureKey = blinded_block.signature().serialize();

        match self.inflight_payloads.entry(sig_key) {
            dashmap::mapref::entry::Entry::Occupied(entry) => {
                trace!("dedup get_payload hit");
                Ok(GetPayloadKind::Dedup(entry.get().clone()))
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let (tx, rx) = oneshot::channel();
                trace!("sending to worker");
                self.worker
                    .try_send(GetPayload {
                        proposer_pubkey,
                        blinded_block: Box::new(blinded_block),
                        trace,
                        res_tx: tx,
                        span: tracing::Span::current(),
                    })
                    .map_err(|_| ChannelFull)?;

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
