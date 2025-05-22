use std::time::Duration;

use helix_common::{api::builder_api::InclusionList, InclusionListConfig};
use reqwest::{Client, ClientBuilder, Url};
use thiserror::Error;
use tracing::{info, warn};

/// Use a short timeout here because we only have 6 seconds to declare an inclusion list.
const IL_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

pub struct InclusionListFetcher {
    http: Client,
    config: InclusionListConfig,
}

impl InclusionListFetcher {
    pub fn new(config: InclusionListConfig) -> Self {
        let http = ClientBuilder::new()
            .timeout(IL_REQUEST_TIMEOUT)
            .build()
            .expect("Failed to build HTTP client for fetching inclusion lists");

        Self { http, config }
    }

    pub async fn fetch_inclusion_list_with_retry(&self, head_slot: u64) -> InclusionList {
        let mut retry_interval = tokio::time::interval(Duration::from_millis(100));

        info!(head_slot = head_slot, "Starting to fetch inclusion list for this slot");
        loop {
            retry_interval.tick().await;

            match self.fetch_inclusion_list(self.config.node.clone()).await {
                Ok(inclusion_list) => return inclusion_list,
                Err(err) => warn!(
                    head_slot = head_slot,
                    "Failed to fetch inclusion list for this slot {}", err
                ),
            }
        }
    }

    async fn fetch_inclusion_list(
        &self,
        node_url: Url,
    ) -> Result<InclusionList, InclusionListError> {
        let bytes = self.http.get(node_url).send().await?.bytes().await?;

        // Quick validity check that the IL is < 8KiB & not empty
        if bytes.is_empty() || bytes.len() > self.config.max_size_bytes {
            return Err(InclusionListError::InvalidSize(bytes.len()));
        }

        let inclusion_list: InclusionList = serde_json::from_slice(&bytes)?;

        Ok(inclusion_list)
    }
}

#[derive(Debug, Error)]
enum InclusionListError {
    #[error("HTTP error. {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Invalid inclusion list size: {0} bytes")]
    InvalidSize(usize),

    #[error("Invalid inclusion list {0}")]
    DeserializeError(#[from] serde_json::Error),
}
