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
}
