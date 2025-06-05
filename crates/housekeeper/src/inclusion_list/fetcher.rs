use async_trait::async_trait;
use helix_common::api::builder_api::InclusionList;

use crate::inclusion_list::http_fetcher::HttpListFetcher;

#[async_trait]
pub trait ListFetcher: Send + Sync + Clone {
    async fn fetch_inclusion_list(&self, slot: u64) -> InclusionList;
}

#[async_trait]
impl ListFetcher for HttpListFetcher {
    async fn fetch_inclusion_list(&self, slot: u64) -> InclusionList {
        self.fetch_inclusion_list_with_retry(slot).await
    }
}
