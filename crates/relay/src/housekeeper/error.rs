use helix_common::local_cache::AuctioneerError;
use thiserror::Error;

use crate::{beacon::error::BeaconClientError, database::error::DatabaseError};

#[derive(Debug, Error)]
pub enum HousekeeperError {
    #[error("beacon client error. {0}")]
    BeaconClient(#[from] BeaconClientError),

    #[error("database error. {0}")]
    Database(#[from] DatabaseError),

    #[error("auctioneer error. {0}")]
    Auctioneer(#[from] AuctioneerError),
}
