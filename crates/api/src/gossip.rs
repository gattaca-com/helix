use std::sync::Arc;

use helix_common::{task, utils::utcnow_ns, GetPayloadTrace};
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::{
    builder::api::BuilderApi, gossiper::types::GossipedMessage, proposer::ProposerApi, Api,
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
                    match proposer
                        ._get_payload(payload.signed_blinded_beacon_block, &mut trace, None)
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
                task::spawn(file!(), line!(), async move {
                    builder.process_gossiped_payload(*payload).await;
                });
            }
        }
    }
}
