pub mod builder_api;
pub mod data_api;
pub mod proposer_api;

pub(crate) const PATH_BUILDER_API: &str = "/relay/v1/builder";

pub(crate) const PATH_GET_VALIDATORS: &str = "/validators";
pub(crate) const PATH_SUBMIT_BLOCK: &str = "/blocks";
pub(crate) const PATH_SUBMIT_BLOCK_OPTIMISTIC_V2: &str = "/blocks_optimistic_v2";
pub(crate) const PATH_SUBMIT_HEADER: &str = "/headers";

pub(crate) const PATH_PROPOSER_API: &str = "/eth/v1/builder";

pub(crate) const PATH_STATUS: &str = "/status";
pub(crate) const PATH_REGISTER_VALIDATORS: &str = "/validators";
pub(crate) const PATH_GET_HEADER: &str = "/header/:slot/:parent_hash/:pubkey";
pub(crate) const PATH_GET_PAYLOAD: &str = "/blinded_blocks";


pub(crate) const PATH_DATA_API: &str = "/relay/v1/data";

pub(crate) const PATH_PROPOSER_PAYLOAD_DELIVERED: &str = "/bidtraces/proposer_payload_delivered";
pub(crate) const PATH_BUILDER_BIDS_RECEIVED: &str = "/bidtraces/builder_blocks_received";
pub(crate) const PATH_VALIDATOR_REGISTRATION: &str = "/validator_registration";