// Auctioneer
pub(crate) const LAST_HASH_DELIVERED_KEY: &str = "last-hash-delivered";
pub(crate) const LAST_SLOT_DELIVERED_KEY: &str = "last-slot-delivered";
pub(crate) const BID_TRACE_KEY: &str = "cache-bid-trace";
pub(crate) const GET_HEADER_RESPONSE_KEY: &str = "cache-gethead-response";
pub(crate) const TOP_BID_VALUE_KEY: &str = "top-bid-value";
pub(crate) const BLOCK_BUILDER_LATEST_BID_KEY: &str = "block-builder-latest-bid";
pub(crate) const BLOCK_BUILDER_LATEST_BID_TIME_KEY: &str = "block-builder-latest-bid-time";
pub(crate) const BLOCK_BUILDER_LATEST_BID_VALUE_KEY: &str = "block-builder-latest-bid-value";
pub(crate) const BID_FLOOR_KEY: &str = "bid-floor";
pub(crate) const BID_FLOOR_VALUE_KEY: &str = "bid-floor-value";
pub(crate) const EXEC_PAYLOAD_KEY: &str = "cache-exec-payload";
pub(crate) const BUILDER_INFO_KEY: &str = "builder-info";
pub(crate) const PROPOSER_WHITELIST_KEY: &str = "proposer-whitelist";
pub(crate) const HOUSEKEEPER_LOCK_KEY: &str = "housekeeper-lock";
pub(crate) const PENDING_BLOCK_KEY: &str = "pending-block";
pub(crate) const PRIMEV_PROPOSERS_KEY: &str = "primev-proposers";
pub(crate) const HEADER_TX_ROOT: &str = "header-tx-root";
pub(crate) const KILL_SWITCH: &str = "kill-switch";
pub(crate) const PAYLOAD_ADDRESS_KEY: &str = "payload-addr";
pub(crate) const CURRENT_INCLUSION_LIST_KEY: &str = "current-inclusion-list";

pub fn inclusion_list_key(slot_coordinate: &str) -> String {
    format!("{CURRENT_INCLUSION_LIST_KEY}_{slot_coordinate}")
}
