#![allow(clippy::too_many_arguments)]

use std::sync::Arc;

use helix_common::{RelayConfig, api_provider::ApiProvider, local_cache::LocalCache};
pub use service::start_api_service;

pub use crate::auctioneer::{BidAdjustor, DefaultBidAdjustor};

pub mod admin_service;
pub mod builder;
pub mod integration_tests;
pub mod middleware;
pub mod proposer;
pub mod relay_data;
pub mod router;
pub mod service;

pub fn start_admin_service(auctioneer: Arc<LocalCache>, config: &RelayConfig) {
    tokio::spawn(admin_service::run_admin_service(auctioneer, config.clone()));
}

pub trait Api: Clone + Send + Sync + 'static {
    type ApiProvider: ApiProvider;
}

pub const HEADER_API_KEY: &str = "x-api-key";
pub const HEADER_API_TOKEN: &str = "x-api-token";
pub const HEADER_SEQUENCE: &str = "x-sequence";
pub const HEADER_HYDRATE: &str = "x-hydrate";
pub const HEADER_IS_MERGEABLE: &str = "x-mergeable";
pub const HEADER_SUBMISSION_TYPE: &str = "x-submission-type";
pub const HEADER_MERGE_TYPE: &str = "x-merge-type";
pub const HEADER_WITH_ADJUSTMENTS: &str = "x-with-adjustments";
