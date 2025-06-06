pub mod broadcast_get_payload;
pub mod broadcast_header;
pub mod broadcast_payload;
pub mod request_payload;

pub use broadcast_get_payload::*;
pub use broadcast_header::*;
pub use broadcast_payload::*;
pub use request_payload::*;

#[derive(Debug, Clone)]
pub enum GossipedMessage {
    Header(Box<BroadcastHeaderParams>),
    Payload(Box<BroadcastPayloadParams>),
    GetPayload(Box<BroadcastGetPayloadParams>),
    RequestPayload(Box<RequestPayloadParams>),
}
