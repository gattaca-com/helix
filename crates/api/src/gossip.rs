use std::sync::Arc;

use helix_common::{task, utils::utcnow_ns, GetPayloadTrace};
use helix_types::PayloadAndBlobsRef;
use tokio::sync::mpsc;
use tracing::{debug, error};
use uuid::Uuid;

use crate::{
    builder::api::BuilderApi,
    gossiper::types::{BroadcastPayloadParams, GossipedMessage},
    proposer::ProposerApi,
    Api,
};

pub async fn process_gossip_messages<A: Api>(
    builder_api: Arc<BuilderApi<A>>,
    proposer_api: Arc<ProposerApi<A>>,
    mut rx: mpsc::Receiver<GossipedMessage>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            GossipedMessage::GetPayload(payload) => {
                let proposer = proposer_api.clone();
                task::spawn(file!(), line!(), async move {
                    let mut trace = GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };
                    debug!(request_id = %payload.request_id, "processing gossiped payload");
                    // TODO: send directly to auctioneer since we already have it decoded
                    match proposer
                        ._get_payload(payload.signed_blinded_beacon_block, &mut trace, None)
                        .await
                    {
                        Ok(_get_payload_response) => {
                            debug!(request_id = %payload.request_id, "gossiped payload processed");
                        }
                        Err(err) => {
                            error!(request_id = %payload.request_id, %err, "error processing gossiped payload");
                        }
                    }
                });
            }
            GossipedMessage::RequestPayload(payload) => {
                let proposer = proposer_api.clone();
                task::spawn(file!(), line!(), async move {
                    let request_id = Uuid::new_v4();
                    debug!(request_id = %request_id, "processing gossiped payload request");
                    let payload_result = proposer
                        .get_execution_payload(
                            payload.slot,
                            &payload.proposer_pub_key,
                            &payload.block_hash,
                            false,
                        )
                        .await;

                    match payload_result {
                        Ok(execution_payload) => {
                            proposer
                                .gossiper
                                .broadcast_payload(BroadcastPayloadParams::to_proto(
                                    PayloadAndBlobsRef::from(&execution_payload),
                                    payload.slot,
                                    &payload.proposer_pub_key,
                                    proposer.chain_info.current_fork_name(),
                                ))
                                .await
                        }
                        Err(err) => {
                            error!(%request_id, %err, "error fetching execution payload");
                        }
                    }
                });
            }

            GossipedMessage::Payload(payload) => {
                let builder = builder_api.clone();
                task::spawn(file!(), line!(), async move {
                    builder.process_gossiped_payload(*payload).await;
                });
            }
        }
    }
}
