pub mod builder_api;
pub mod constraints_api;
pub mod data_api;
pub mod proposer_api;

pub(crate) const PATH_BUILDER_API: &str = "/relay/v1/builder";

pub(crate) const PATH_GET_VALIDATORS: &str = "/validators";
pub(crate) const PATH_SUBMIT_BLOCK: &str = "/blocks";
pub(crate) const PATH_BUILDER_BLOCKS_WITH_PROOFS: &str = "/blocks_with_proofs";
pub(crate) const PATH_SUBMIT_BLOCK_OPTIMISTIC_V2: &str = "/blocks_optimistic_v2";
pub(crate) const PATH_CANCEL_BID: &str = "/cancel_bid";
pub(crate) const PATH_SUBMIT_HEADER: &str = "/headers";
pub(crate) const PATH_GET_TOP_BID: &str = "/top_bid";
pub(crate) const PATH_BUILDER_CONSTRAINTS: &str = "/constraints";
pub(crate) const PATH_BUILDER_CONSTRAINTS_STREAM: &str = "/constraints_stream";
pub(crate) const PATH_BUILDER_DELEGATIONS: &str = "/delegations";

pub(crate) const PATH_PROPOSER_API: &str = "/eth/v1/builder";

pub(crate) const PATH_STATUS: &str = "/status";
pub(crate) const PATH_REGISTER_VALIDATORS: &str = "/validators";
pub(crate) const PATH_GET_HEADER: &str = "/header/:slot/:parent_hash/:pubkey";
pub(crate) const PATH_GET_PAYLOAD: &str = "/blinded_blocks";
pub(crate) const PATH_GET_HEADER_WITH_PROOFS: &str =
    "/header_with_proofs/:slot/:parent_hash/:pubkey";

pub(crate) const PATH_DATA_API: &str = "/relay/v1/data";

pub(crate) const PATH_PROPOSER_PAYLOAD_DELIVERED: &str = "/bidtraces/proposer_payload_delivered";
pub(crate) const PATH_BUILDER_BIDS_RECEIVED: &str = "/bidtraces/builder_blocks_received";
pub(crate) const PATH_VALIDATOR_REGISTRATION: &str = "/validator_registration";

pub(crate) const PATH_CONSTRAINTS_API: &str = "/constraints/v1";

pub(crate) const PATH_SUBMIT_BUILDER_CONSTRAINTS: &str = "/builder/constraints";
pub(crate) const PATH_DELEGATE_SUBMISSION_RIGHTS: &str = "/builder/delegate";
pub(crate) const PATH_REVOKE_SUBMISSION_RIGHTS: &str = "/builder/revoke";
