use std::{hash::Hash, sync::Arc};
use std::collections::HashMap;
use helix_common::{task, utils::utcnow_ns, GetPayloadTrace};
use helix_types::BlsPublicKey;
use tokio::sync::{mpsc, Semaphore};
use tower::builder;
use tracing::{debug, error};
use uuid::Uuid;

use crate::{
    builder::api::BuilderApi,
    gossiper::types::{BroadcastPayloadParams, GossipedMessage},
    proposer::ProposerApi,
    Api,
};

const MAX_TASKS_PER_BUILDER: usize = 100;

pub async fn process_gossip_messages<A: Api>(
    builder_api: Arc<BuilderApi<A>>,
    proposer_api: Arc<ProposerApi<A>>,
    mut rx: mpsc::Receiver<GossipedMessage>,
) {
    let mut builder_semaphores = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            GossipedMessage::GetPayload(payload) => {
                let proposer = proposer_api.clone();
                task::spawn(file!(), line!(), async move {
                    let mut trace = GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };
                    debug!(request_id = %payload.request_id, "processing gossiped payload");
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
                                .broadcast_payload(BroadcastPayloadParams {
                                    execution_payload,
                                    slot: payload.slot,
                                    proposer_pub_key: payload.proposer_pub_key,
                                })
                                .await
                        }
                        Err(err) => {
                            error!(%request_id, %err, "error fetching execution payload");
                        }
                    }
                });
            }
            GossipedMessage::Header(header) => {
                let builder = builder_api.clone();
                
                let head_slot = builder.get_current_head_slot();
                if header.slot() <= head_slot {
                    debug!("received gossiped header for a past slot");
                    return;
                }

                let semaphore = builder_semaphores
                    .entry(header.builder_pubkey().clone())
                    .or_insert_with(|| Arc::new(Semaphore::new(MAX_TASKS_PER_BUILDER)))
                    .clone();

                if let Ok(permit) = semaphore.try_acquire_owned() {
                    task::spawn(file!(), line!(), async move {
                        builder.process_gossiped_header(*header).await;
                        drop(permit);
                    });
                } else {
                    debug!(builder = ?header.builder_pubkey(), "dropping header due to rate limit");
                }
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
