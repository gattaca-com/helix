use helix_common::{GetPayloadTrace, SubmissionTrace};
use helix_types::{BlsPublicKeyBytes, SignedBlindedBeaconBlock, Slot};
use http::HeaderMap;
use tokio::sync::oneshot;

use crate::{
    auctioneer::types::{
        BestMergeablePayload, Event, GetHeaderResult, GetPayloadResult, SubmissionResult, WorkerJob,
    },
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

    #[allow(clippy::result_unit_err)]
    pub fn block_submission(
        &self,
        headers: HeaderMap,
        body: bytes::Bytes,
        trace: SubmissionTrace,
    ) -> Result<oneshot::Receiver<SubmissionResult>, ()> {
        let (tx, rx) = oneshot::channel();
        self.worker
            .try_send(WorkerJob::BlockSubmission { headers, body, trace, res_tx: tx })
            .map_err(|_| ())?;
        Ok(rx)
    }

    #[allow(clippy::result_unit_err)]
    pub fn get_header(
        &self,
        params: GetHeaderParams,
    ) -> Result<oneshot::Receiver<GetHeaderResult>, ()> {
        let (tx, rx) = oneshot::channel();
        self.auctioneer.try_send(Event::GetHeader { params, res_tx: tx }).map_err(|_| ())?;
        Ok(rx)
    }

    #[allow(clippy::result_unit_err)]
    pub fn get_payload(
        &self,
        proposer_pubkey: BlsPublicKeyBytes,
        blinded_block: SignedBlindedBeaconBlock,
        trace: GetPayloadTrace,
    ) -> Result<oneshot::Receiver<GetPayloadResult>, ()> {
        let (tx, rx) = oneshot::channel();
        self.worker
            .try_send(WorkerJob::GetPayload { proposer_pubkey, blinded_block, trace, res_tx: tx })
            .map_err(|_| ())?;
        Ok(rx)
    }

    #[allow(clippy::result_unit_err)]
    pub fn gossip_payload(&self, req: BroadcastPayloadParams) -> Result<(), ()> {
        self.auctioneer.try_send(Event::GossipPayload(req)).map_err(|_| ())
    }

    #[allow(clippy::result_unit_err)]
    pub fn best_mergeable(
        &self,
        bid_slot: Slot,
    ) -> Result<oneshot::Receiver<BestMergeablePayload>, ()> {
        let (tx, rx) = oneshot::channel();
        self.auctioneer
            .try_send(Event::GetBestPayloadForMerging { bid_slot, res_tx: tx })
            .map_err(|_| ())?;

        Ok(rx)
    }
}
