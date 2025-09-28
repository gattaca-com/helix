use crossbeam_channel::TrySendError;
use helix_common::{GetPayloadTrace, SubmissionTrace};
use helix_types::SignedBlindedBeaconBlock;
use http::HeaderMap;
use tokio::sync::oneshot;

use crate::{
    auctioneer::types::{Event, GetHeaderResult, GetPayloadResult, SubmissionResult, WorkerJob},
    gossiper::types::BroadcastPayloadParams,
    proposer::GetHeaderParams,
};

#[derive(Clone)]
pub struct AuctioneerHandle {
    worker: crossbeam_channel::Sender<WorkerJob>,
    auctioneer: crossbeam_channel::Sender<Event>,
}

impl AuctioneerHandle {
    pub fn new(
        worker: crossbeam_channel::Sender<WorkerJob>,
        auctioneer: crossbeam_channel::Sender<Event>,
    ) -> Self {
        Self { worker, auctioneer }
    }

    pub fn block_submission(
        &self,
        headers: HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace,
    ) -> Result<oneshot::Receiver<SubmissionResult>, TrySendError<WorkerJob>> {
        let (tx, rx) = oneshot::channel();
        self.worker.try_send(WorkerJob::BlockSubmission { headers, body, trace, res_tx: tx })?;
        Ok(rx)
    }

    pub fn get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<oneshot::Receiver<GetHeaderResult>, TrySendError<Event>> {
        let (tx, rx) = oneshot::channel();
        self.auctioneer.try_send(Event::GetHeader { params, res_tx: tx })?;
        Ok(rx)
    }

    pub fn get_payload(
        &self,
        blinded_block: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
    ) -> Result<oneshot::Receiver<GetPayloadResult>, TrySendError<WorkerJob>> {
        let (tx, rx) = oneshot::channel();
        self.worker.try_send(WorkerJob::GetPayload { blinded_block, trace, res_tx: tx })?;
        Ok(rx)
    }

    pub fn gossip_payload(&self, req: BroadcastPayloadParams) -> Result<(), TrySendError<Event>> {
        self.auctioneer.try_send(Event::GossipPayload(req))
    }
}
