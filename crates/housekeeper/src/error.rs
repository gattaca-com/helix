use helix_beacon::error::BeaconClientError;
use helix_common::local_cache::AuctioneerError;
use helix_database::error::DatabaseError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HousekeeperError {
    #[error("beacon client error. {0}")]
    BeaconClientError(#[from] BeaconClientError),

    #[error("database error. {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("auctioneer error. {0}")]
    AuctioneerError(#[from] AuctioneerError),
}
