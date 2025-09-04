pub mod builder_api;
pub mod data_api;
pub mod proposer_api;

pub const PATH_BUILDER_API: &str = "/relay/v1/builder";
pub const PATH_BUILDER_API_V3: &str = "/relay/v3/builder";
pub const PATH_GET_VALIDATORS: &str = "/validators";
pub const PATH_SUBMIT_BLOCK: &str = "/blocks";
pub const PATH_CANCEL_BID: &str = "/cancel_bid";
pub const PATH_SUBMIT_HEADER: &str = "/headers";
pub const PATH_GET_TOP_BID: &str = "/top_bid";
pub const PATH_GET_INCLUSION_LIST: &str = "/inclusion_list/{slot}/{parent_hash}/{pub_key}";

pub const PATH_PROPOSER_API: &str = "/eth/v1/builder";

pub const PATH_STATUS: &str = "/status";
pub const PATH_REGISTER_VALIDATORS: &str = "/validators";
pub const PATH_GET_HEADER: &str = "/header/{slot}/{parent_hash}/{pubkey}";
pub const PATH_GET_PAYLOAD: &str = "/blinded_blocks";

pub const PATH_DATA_API: &str = "/relay/v1/data";

pub const PATH_PROPOSER_PAYLOAD_DELIVERED: &str = "/bidtraces/proposer_payload_delivered";
pub const PATH_BUILDER_BIDS_RECEIVED: &str = "/bidtraces/builder_blocks_received";
pub const PATH_VALIDATOR_REGISTRATION: &str = "/validator_registration";

pub const PATH_P2P: &str = "/relay/v1/p2p";
