// Auctioneer
pub(crate) const BID_TRACE_KEY: &str = "cache-bid-trace";
pub(crate) const EXEC_PAYLOAD_KEY: &str = "cache-exec-payload";
pub(crate) const CURRENT_INCLUSION_LIST_KEY: &str = "current-inclusion-list";

pub fn inclusion_list_key(slot_coordinate: &str) -> String {
    format!("{CURRENT_INCLUSION_LIST_KEY}_{slot_coordinate}")
}
