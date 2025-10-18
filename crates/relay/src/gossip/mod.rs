mod client;
mod error;
mod types;
mod grpc {
    #![allow(clippy::all)]
    include!("./generated/gossip.rs");
}

pub use client::GrpcGossiperClientManager;
pub use types::{BroadcastGetPayloadParams, BroadcastPayloadParams};

use std::sync::Arc;

use helix_common::{GetPayloadTrace, spawn_tracked, utils::utcnow_ns};
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::{
    api::{
        Api,
        builder::api::BuilderApi,
        proposer::{ProposerApi, get_payload::ProposerApiVersion},
    },
    gossip::types::GossipedMessage,
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
                spawn_tracked!(async move {
                    let mut trace = GetPayloadTrace { receive: utcnow_ns(), ..Default::default() };
                    debug!(request_id = %payload.request_id, "processing gossiped payload");
                    match proposer
                        ._get_payload(
                            payload.signed_blinded_beacon_block,
                            &mut trace,
                            None,
                            ProposerApiVersion::V1,
                        )
                        .await
                    {
                        Ok(_) => {
                            debug!(request_id = %payload.request_id, "gossiped payload processed")
                        }
                        Err(err) => {
                            error!(request_id = %payload.request_id, %err, "error processing gossiped payload");
                        }
                    }
                });
            }

            GossipedMessage::Payload(payload) => {
                let builder = builder_api.clone();
                spawn_tracked!(async move {
                    builder.process_gossiped_payload(*payload).await;
                });
            }
        }
    }
}
