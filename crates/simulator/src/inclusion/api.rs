use alloy_primitives::Bytes;
use jsonrpsee::{
    PendingSubscriptionSink, SubscriptionMessage,
    core::{RpcResult, SubscriptionResult},
    proc_macros::rpc,
};
use reth_ethereum::rpc::eth::error::RpcPoolError;
use tokio::sync::watch::Receiver;

/// trait interface for a custom rpc namespace: `relay`
///
/// This defines an additional namespace where all methods are configured as trait functions.
#[rpc(server, namespace = "relay")]
pub trait InclusionExtApi {
    /// Returns the current inclusion list.
    #[method(name = "inclusionList")]
    fn inclusion_list(&self) -> RpcResult<Vec<Bytes>>;

    /// Creates a subscription that returns the inclusion list when it is published.
    #[subscription(name = "subscribeInclusionList", item = usize)]
    fn subscribe_inclusion_list(&self) -> SubscriptionResult;
}

/// The type that implements the `inclusion` rpc namespace trait
pub struct InclusionExt {
    pub published: Receiver<Option<Vec<Bytes>>>,
}

impl InclusionExtApiServer for InclusionExt {
    fn inclusion_list(&self) -> RpcResult<Vec<Bytes>> {
        match self.published.borrow().clone() {
            Some(list) => RpcResult::Ok(list),
            None => RpcResult::Err(RpcPoolError::Other("list not ready".into()).into()),
        }
    }

    fn subscribe_inclusion_list(
        &self,
        pending_subscription_sink: PendingSubscriptionSink,
    ) -> SubscriptionResult {
        let mut published = self.published.clone();
        tokio::spawn(async move {
            let sink = match pending_subscription_sink.accept().await {
                Ok(sink) => sink,
                Err(e) => {
                    println!("failed to accept subscription: {e}");
                    return;
                }
            };

            loop {
                match published.changed().await {
                    Ok(_) => {
                        let msg = published.borrow_and_update().clone().and_then(|list| {
                            match SubscriptionMessage::new(
                                sink.method_name(),
                                sink.subscription_id(),
                                &list,
                            ) {
                                Ok(msg) => Some(msg),
                                Err(e) => {
                                    tracing::error!(error=?e, "could not serialize inclusion list");
                                    None
                                }
                            }
                        });
                        if let Some(msg) = msg {
                            let _ = sink.send(msg).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=?e, "list publisher closed - exiting");
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}
