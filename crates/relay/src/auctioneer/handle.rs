use std::{ops::Range, sync::Arc};

use helix_common::{GetPayloadTrace, api::proposer_api::GetHeaderParams};
use helix_types::{BlsPublicKeyBytes, SignedBlindedBeaconBlock, SignedValidatorRegistration};
use tokio::sync::oneshot;
use tracing::trace;

use crate::{
    auctioneer::types::{Event, GetHeaderResult, GetPayload, GetPayloadResult, RegWorkerJob},
    gossip::BroadcastPayloadParams,
};

#[derive(Clone)]
pub struct AuctioneerHandle {
    worker: crossbeam_channel::Sender<GetPayload>,
    auctioneer: crossbeam_channel::Sender<Event>,
}

impl AuctioneerHandle {
    pub fn new(
        worker: crossbeam_channel::Sender<GetPayload>,
        auctioneer: crossbeam_channel::Sender<Event>,
    ) -> Self {
        Self { worker, auctioneer }
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
    ) -> Result<oneshot::Receiver<GetPayloadResult>, ChannelFull> {
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
        Ok(rx)
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
