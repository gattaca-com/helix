use std::{ops::Range, sync::Arc, time::Instant};

use helix_common::{GetPayloadTrace, SubmissionTrace};
use helix_types::{BlsPublicKeyBytes, SignedBlindedBeaconBlock, SignedValidatorRegistration, Slot};
use http::HeaderMap;
use tokio::sync::oneshot;

use crate::{
    auctioneer::types::{
        BestMergeablePayload, Event, GetHeaderResult, GetPayloadResult, RegWorkerJob, SubWorkerJob,
        SubmissionResult,
    },
    gossiper::types::BroadcastPayloadParams,
    proposer::GetHeaderParams,
};

#[derive(Clone)]
pub struct AuctioneerHandle {
    worker: crossbeam_channel::Sender<SubWorkerJob>,
    auctioneer: crossbeam_channel::Sender<Event>,
}

impl AuctioneerHandle {
    pub fn new(
        worker: crossbeam_channel::Sender<SubWorkerJob>,
        auctioneer: crossbeam_channel::Sender<Event>,
    ) -> Self {
        Self { worker, auctioneer }
    }

    pub fn block_submission(
        &self,
        headers: HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace,
    ) -> Result<oneshot::Receiver<SubmissionResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.worker
            .try_send(SubWorkerJob::BlockSubmission {
                headers,
                body,
                trace,
                res_tx: tx,
                span: tracing::Span::current(),
                sent_at: Instant::now(),
            })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<oneshot::Receiver<GetHeaderResult>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.auctioneer
            .try_send(Event::GetHeader { params, res_tx: tx })
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
        self.worker
            .try_send(SubWorkerJob::GetPayload {
                proposer_pubkey,
                blinded_block,
                trace,
                res_tx: tx,
            })
            .map_err(|_| ChannelFull)?;
        Ok(rx)
    }

    pub fn gossip_payload(&self, req: BroadcastPayloadParams) -> Result<(), ChannelFull> {
        self.auctioneer.try_send(Event::GossipPayload(req)).map_err(|_| ChannelFull)
    }

    pub fn best_mergeable(
        &self,
        bid_slot: Slot,
    ) -> Result<oneshot::Receiver<BestMergeablePayload>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.auctioneer
            .try_send(Event::GetBestPayloadForMerging { bid_slot, res_tx: tx })
            .map_err(|_| ChannelFull)?;

        Ok(rx)
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
