use std::{ops::Range, sync::Arc, time::Instant};

use helix_common::{GetPayloadTrace, SubmissionTrace, api::proposer_api::GetHeaderParams};
use helix_types::{
    BlsPublicKeyBytes, Compression, SignedBlindedBeaconBlock, SignedValidatorRegistration,
};
use tokio::sync::oneshot;
use tracing::trace;

use crate::{
    auctioneer::{
        decoder::Encoding,
        types::{
            BlockSubResultSender, Event, GetHeaderResult, GetPayloadResult, RegWorkerJob,
            SubWorkerJob, SubmissionRef, SubmissionResult,
        },
    },
    gossip::BroadcastPayloadParams,
    tcp_bid_recv::BidSubmissionHeader,
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
        submission_ref: SubmissionRef,
        header: BidSubmissionHeader,
        encoding: Encoding,
        compression: Compression,
        api_key: Option<String>,
        body: bytes::Bytes,
        trace: SubmissionTrace,
        res_tx: BlockSubResultSender<SubmissionResult>,
        expected_pubkey: Option<BlsPublicKeyBytes>,
    ) -> Result<(), ChannelFull> {
        trace!("sending to worker");
        self.worker
            .try_send(SubWorkerJob::BlockSubmission {
                submission_ref,
                header,
                encoding,
                compression,
                api_key,
                body,
                trace,
                res_tx,
                span: tracing::Span::current(),
                sent_at: Instant::now(),
                expected_pubkey,
            })
            .map_err(|_| ChannelFull)
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
            .try_send(SubWorkerJob::GetPayload {
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
