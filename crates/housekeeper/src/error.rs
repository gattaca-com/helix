use helix_beacon_client::error::BeaconClientError;
use helix_database::error::DatabaseError;
use helix_datastore::error::AuctioneerError;
use thiserror::Error;
use tokio::sync::TryLockError;

#[derive(Debug, Error)]
pub enum HousekeeperError {
    #[error("beacon client error. {0}")]
    BeaconClientError(#[from] BeaconClientError),

    #[error("already updating")]
    AlreadyUpdating(#[from] TryLockError),

    #[error("database error. {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("auctioneer error. {0}")]
    AuctioneerError(#[from] AuctioneerError),
}
