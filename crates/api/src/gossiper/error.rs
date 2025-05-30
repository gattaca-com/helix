use helix_types::CryptoError;

#[derive(Debug, thiserror::Error)]
pub enum GossipError {
    #[error("Client is not connected")]
    ClientNotConnected,

    #[error("Failed to broadcast block: {0}")]
    BroadcastError(#[from] tonic::Status),

    #[error("Failed to reconnect")]
    ReconnectFailed,
    // Add other error common as needed
    #[error("Broadcast timed out")]
    TimeoutError,

    #[error("Ssz decode error: {0:?}")]
    SszDecodeError(ssz::DecodeError),

    #[error("Json decode error: {0:?}")]
    JsonDecodeError(#[from] serde_json::Error),

    #[error("Uuid decode error: {0:?}")]
    UuidDecodeError(#[from] uuid::Error),

    #[error("Bls decode error: {0:?}")]
    BlsDecodeError(CryptoError),

    #[error("Slice decode error: {0:?}")]
    SliceDecodeError(#[from] std::array::TryFromSliceError),
}
