use std::fmt::{Display, Formatter};

use async_channel::{Receiver, RecvError, SendError, Sender, TryRecvError, TrySendError, bounded};
use helix_types::OperatorMessage;
use libp2p::{
    BehaviourBuilderError, Multiaddr, TransportError,
    identity::{Keypair, PublicKey},
    multiaddr,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::AbortHandle;

mod pubsub;
mod utils;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Operator {
    pub name: String,
    #[serde(
        serialize_with = "utils::serialize_pubkey",
        deserialize_with = "utils::deserialize_pubkey"
    )]
    pub pubkey: PublicKey,
    pub multiaddr: Multiaddr,
}

#[derive(Debug, Error)]
pub enum OperatorError {
    MultiaddrParseError(#[from] multiaddr::Error),
    SwarmNetworkError(#[from] TransportError<std::io::Error>),
    SwarmBuildError(#[from] BehaviourBuilderError),
    MessageSendError(#[from] SendError<OperatorMessage>),
    MessageTrySendError(#[from] TrySendError<OperatorMessage>),
    MessageRecvError(#[from] RecvError),
    MessageTryRecvError(#[from] TryRecvError),
}

impl Display for OperatorError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), std::fmt::Error> {
        f.write_fmt(format_args!("{self:?}"))
    }
}

/// Handle to operator pubsub.
/// When this is first created, current builder collateral and demotions messages should
/// be sent. These are cached internally (and updated) and sent automatically when new
/// peers connect. 
pub struct OperatorPubSub {
    outgoing_msgs: Sender<OperatorMessage>,
    incoming_msgs: Receiver<OperatorMessage>,
    task_handle: AbortHandle,
}

impl Drop for OperatorPubSub {
    fn drop(&mut self) {
        self.task_handle.abort();
    }
}

impl OperatorPubSub {
    pub fn new(quic_port: u16, local_keypair: Keypair, operators: Vec<Operator>) -> Self {
        let (outgoing_msgs, out_recv) = bounded(128);
        let (in_send, incoming_msgs) = bounded(128);

        // p2p task.
        let handle = tokio::spawn(pubsub::run_operator_connection(
            quic_port,
            local_keypair,
            operators,
            out_recv,
            in_send,
        ));

        Self { outgoing_msgs, incoming_msgs, task_handle: handle.abort_handle() }
    }

    pub async fn send(&self, msg: OperatorMessage) -> Result<(), OperatorError> {
        Ok(self.outgoing_msgs.send(msg).await?)
    }

    pub fn try_send(&self, msg: OperatorMessage) -> Result<(), OperatorError> {
        Ok(self.outgoing_msgs.try_send(msg)?)
    }

    pub async fn recv(&self) -> Result<OperatorMessage, OperatorError> {
        Ok(self.incoming_msgs.recv().await?)
    }

    pub fn try_recv(&self) -> Result<Option<OperatorMessage>, OperatorError> {
        match self.incoming_msgs.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy_primitives::B256;
    use helix_types::{BlsPublicKeyBytes, Demotion, OperatorMessage, Promotion};
    use libp2p::{Multiaddr, identity::Keypair};

    use crate::{Operator, OperatorPubSub};

    #[tokio::test]
    async fn operator_p2p() {
        let keypair_a = Keypair::generate_secp256k1();
        let keypair_b = Keypair::generate_secp256k1();

        let operator_a = Operator {
            name: "operator A".into(),
            pubkey: keypair_a.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/23032/quic-v1").unwrap(),
        };

        let operator_b = Operator {
            name: "operator B".into(),
            pubkey: keypair_b.public(),
            multiaddr: Multiaddr::from_str("/ip4/127.0.0.1/udp/32023/quic-v1").unwrap(),
        };

        let op_a = OperatorPubSub::new(23032, keypair_a, vec![operator_b]);
        let op_b = OperatorPubSub::new(32023, keypair_b, vec![operator_a]);

        let builder_pubkey = BlsPublicKeyBytes::random();
        let demotion = Demotion {
            ts_ms: 1,
            slot: 1,
            builder_pubkey,
            block_hash: B256::random(),
            reason_msg: "fail".as_bytes().to_vec(),
        };
        let promotion = Promotion { ts_ms: 2, slot: 2, builder_pubkey };
        op_a.send(helix_types::OperatorMessage::Demotion(demotion)).await.unwrap();
        let msg = op_b.recv().await.unwrap();
        assert!(matches!(msg, OperatorMessage::Demotion(_)));

        op_b.send(OperatorMessage::Promotion(promotion)).await.unwrap();
        let msg = op_a.recv().await.unwrap();
        assert!(matches!(msg, OperatorMessage::Promotion(_)));
    }
}
