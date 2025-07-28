// Auctioneer
pub(crate) const LAST_HASH_DELIVERED_KEY: &str = "last-hash-delivered";
pub(crate) const LAST_SLOT_DELIVERED_KEY: &str = "last-slot-delivered";
pub(crate) const BLOCK_MERGING_DATA_KEY: &str = "cache-block-merging-data";
pub(crate) const BID_TRACE_KEY: &str = "cache-bid-trace";
pub(crate) const EXEC_PAYLOAD_KEY: &str = "cache-exec-payload";
pub(crate) const BUILDER_INFO_KEY: &str = "builder-info";
pub(crate) const PROPOSER_WHITELIST_KEY: &str = "proposer-whitelist";
pub(crate) const HOUSEKEEPER_LOCK_KEY: &str = "housekeeper-lock";
pub(crate) const PRIMEV_PROPOSERS_KEY: &str = "primev-proposers";
pub(crate) const KILL_SWITCH: &str = "kill-switch";
pub(crate) const PAYLOAD_ADDRESS_KEY: &str = "payload-addr";
pub(crate) const CURRENT_INCLUSION_LIST_KEY: &str = "current-inclusion-list";

pub fn inclusion_list_key(slot_coordinate: &str) -> String {
    format!("{CURRENT_INCLUSION_LIST_KEY}_{slot_coordinate}")
}
