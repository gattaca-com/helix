use helix_common::{
    bid_submission::BidSubmission, metadata_provider::MetadataProvider,
    metrics::BUILDER_GOSSIP_QUEUE, task, utils::utcnow_ns, GossipedHeaderTrace,
    GossipedPayloadTrace,
};
use helix_database::DatabaseService;
use helix_datastore::{types::SaveBidAndUpdateTopBidResponse, Auctioneer};
use helix_types::{BidTrace, PayloadAndBlobs, SignedBidSubmission, SignedBuilderBid};
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::api::BuilderApi;
use crate::gossiper::types::{BroadcastHeaderParams, BroadcastPayloadParams, GossipedMessage};

impl<A, DB, MP> BuilderApi<A, DB, MP>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    MP: MetadataProvider + 'static,
{
}

// Handle Gossiped Payloads
impl<A, DB, MP> BuilderApi<A, DB, MP>
where
    A: Auctioneer + 'static,
    DB: DatabaseService + 'static,
    MP: MetadataProvider + 'static,
{
    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_header(&self, req: BroadcastHeaderParams) {
        let block_hash = req.signed_builder_bid.data.message.header().block_hash().0;
        debug!(?block_hash, "received gossiped header");

        let mut trace = GossipedHeaderTrace {
            on_receive: req.on_receive,
            on_gossip_receive: utcnow_ns(),
            ..Default::default()
        };

        // Verify that the gossiped header is not for a past slot
        let head_slot = self.curr_slot_info.head_slot();
        if req.slot <= head_slot.as_u64() {
            debug!("received gossiped header for a past slot");
            return;
        }

        // Handle duplicates.
        if self
            .check_for_duplicate_block_hash(
                &block_hash,
                req.slot,
                &req.parent_hash,
                &req.proposer_pub_key,
            )
            .await
            .is_err()
        {
            return;
        }

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if req.slot <= slot {
                    debug!("payload already delivered");
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(%err, "failed to get last slot delivered");
            }
        }

        // Verify the bid value is above the floor bid
        let floor_bid_value = match self
            .check_if_bid_is_below_floor(
                req.slot,
                &req.parent_hash,
                &req.proposer_pub_key,
                &req.builder_pub_key,
                *req.signed_builder_bid.data.message.value(),
                req.is_cancellations_enabled,
            )
            .await
        {
            Ok(floor_bid_value) => floor_bid_value,
            Err(err) => {
                warn!(%err, "bid is below floor");
                return;
            }
        };

        trace.pre_checks = utcnow_ns();

        // Save header to auctioneer
        let mut update_bid_result = SaveBidAndUpdateTopBidResponse::default();
        if let Err(err) = self
            .auctioneer
            .save_signed_builder_bid_and_update_top_bid(
                &req.signed_builder_bid,
                &req.bid_trace,
                req.on_receive.into(),
                req.is_cancellations_enabled,
                floor_bid_value,
                &mut update_bid_result,
            )
            .await
        {
            warn!(%err, "failed to save header bid");
            return;
        }

        if let Some(payload_address) = req.payload_address {
            if let Err(e) = self
                .auctioneer
                .save_payload_address(
                    &req.bid_trace.block_hash,
                    &req.builder_pub_key,
                    payload_address,
                )
                .await
            {
                warn!(%e, "failed to save payload address");
            }
        }

        trace.auctioneer_update = utcnow_ns();

        debug!("succesfully saved gossiped header");

        // Save latency trace to db
        // let db = self.db.clone();
        // task::spawn(file!(), line!(), async move {
        //     if let Err(err) = db
        //         .save_gossiped_header_trace(req.bid_trace.block_hash.clone(), Arc::new(trace))
        //         .await
        //     {
        //         error!(%err, "failed to store gossiped header trace")
        //     }
        // });
    }

    #[tracing::instrument(skip_all, fields(id = %Uuid::new_v4()))]
    pub async fn process_gossiped_payload(&self, req: BroadcastPayloadParams) {
        let block_hash = req.execution_payload.execution_payload.block_hash().0;

        debug!(?block_hash, "received gossiped payload");

        let mut trace = GossipedPayloadTrace { receive: utcnow_ns(), ..Default::default() };

        // Verify that the gossiped payload is not for a past slot
        let head_slot = self.curr_slot_info.head_slot();
        if req.slot <= head_slot.as_u64() {
            debug!("received gossiped payload for a past slot");
            return;
        }

        // Verify payload has not already been delivered
        match self.auctioneer.get_last_slot_delivered().await {
            Ok(Some(slot)) => {
                if req.slot <= slot {
                    debug!("payload already delivered");
                    return;
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!(%err, "failed to get last slot delivered");
            }
        }

        trace.pre_checks = utcnow_ns();

        // Save payload to auctioneer
        if let Err(err) = self
            .auctioneer
            .save_execution_payload(
                req.slot,
                &req.proposer_pub_key,
                &req.execution_payload.execution_payload.block_hash().0,
                &req.execution_payload,
            )
            .await
        {
            error!(%err, "failed to save execution payload");
            return;
        }

        trace.auctioneer_update = utcnow_ns();

        debug!("succesfully saved gossiped payload");

        // Save gossiped payload trace to db
        let db = self.db.clone();
        task::spawn(file!(), line!(), async move {
            if let Err(err) = db.save_gossiped_payload_trace(block_hash, trace).await {
                error!(%err, "failed to store gossiped payload trace")
            }
        });
    }

    /// This function should be run as a seperate async task.
    /// Will process new gossiped messages from
    pub(crate) async fn process_gossiped_info(&self, mut recveiver: Receiver<GossipedMessage>) {
        while let Some(msg) = recveiver.recv().await {
            BUILDER_GOSSIP_QUEUE.dec();
            match msg {
                GossipedMessage::Header(header) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        api_clone.process_gossiped_header(*header).await;
                    });
                }
                GossipedMessage::Payload(payload) => {
                    let api_clone = self.clone();
                    task::spawn(file!(), line!(), async move {
                        api_clone.process_gossiped_payload(*payload).await;
                    });
                }
                _ => {}
            }
        }
    }

    async fn _gossip_new_submission(
        &self,
        payload: &SignedBidSubmission,
        execution_payload: PayloadAndBlobs,
        builder_bid: SignedBuilderBid,
        is_cancellations_enabled: bool,
        on_receive: u64,
    ) {
        self.gossip_header(
            builder_bid,
            payload.bid_trace(),
            is_cancellations_enabled,
            on_receive,
            None,
        )
        .await;
        self.gossip_payload(payload, execution_payload).await;
    }

    pub(crate) async fn gossip_header(
        &self,
        builder_bid: SignedBuilderBid,
        bid_trace: &BidTrace,
        is_cancellations_enabled: bool,
        on_receive: u64,
        payload_address: Option<Vec<u8>>,
    ) {
        let params = BroadcastHeaderParams {
            signed_builder_bid: builder_bid,
            bid_trace: bid_trace.clone(),
            slot: bid_trace.slot,
            parent_hash: bid_trace.parent_hash,
            proposer_pub_key: bid_trace.proposer_pubkey.clone(),
            builder_pub_key: bid_trace.builder_pubkey.clone(),
            is_cancellations_enabled,
            on_receive,
            payload_address,
        };
        self.gossiper.broadcast_header(params).await
    }

    pub(crate) async fn gossip_payload(
        &self,
        payload: &SignedBidSubmission,
        execution_payload: PayloadAndBlobs,
    ) {
        let params = BroadcastPayloadParams {
            execution_payload,
            slot: payload.slot().as_u64(),
            proposer_pub_key: payload.proposer_public_key().clone(),
        };
        self.gossiper.broadcast_payload(params).await
    }
}
