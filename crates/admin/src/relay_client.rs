use std::time::Duration;

use axum::http::StatusCode;
use helix_types::BlsPublicKeyBytes;
use serde::{Deserialize, Serialize};

use crate::error::AdminApiError;

#[derive(Deserialize, Serialize, Clone, Copy)]
pub struct RelayAdminStatus {
    pub kill_switch_enabled: bool,
}

#[derive(Serialize)]
struct DemoteRequest {
    reason: Option<String>,
}

/// Client for the relay's bearer-token admin API (see
/// `helix-relay::api::admin_service`).
#[derive(Clone)]
pub struct RelayAdminClient {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl RelayAdminClient {
    pub fn new(base: impl Into<String>, token: String) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build relay admin client");
        Self { base, token, client }
    }

    pub async fn status(&self) -> Result<RelayAdminStatus, AdminApiError> {
        let url = format!("{}/admin/v1/status", self.base);
        let response = self.client.get(url).bearer_auth(&self.token).send().await?;
        Self::check_status(&response)?;
        Ok(response.json().await?)
    }

    pub async fn set_kill_switch(&self, enabled: bool) -> Result<(), AdminApiError> {
        let url = format!("{}/admin/v1/killswitch", self.base);
        let request = if enabled { self.client.post(url) } else { self.client.delete(url) };
        let response = request.bearer_auth(&self.token).send().await?;
        Self::check_status(&response)
    }

    pub async fn demote_builder(
        &self,
        pubkey: &BlsPublicKeyBytes,
        reason: Option<String>,
    ) -> Result<(), AdminApiError> {
        let url = format!("{}/admin/v1/builders/{pubkey}/demote", self.base);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .json(&DemoteRequest { reason })
            .send()
            .await?;
        Self::check_status(&response)
    }

    pub async fn promote_builder(&self, pubkey: &BlsPublicKeyBytes) -> Result<(), AdminApiError> {
        let url = format!("{}/admin/v1/builders/{pubkey}/promote", self.base);
        let response = self.client.post(url).bearer_auth(&self.token).send().await?;
        Self::check_status(&response)
    }

    pub async fn disable_adjustments(&self) -> Result<(), AdminApiError> {
        let url = format!("{}/admin/v1/adjustments/disable", self.base);
        let response = self.client.post(url).bearer_auth(&self.token).send().await?;
        Self::check_status(&response)
    }

    fn check_status(response: &reqwest::Response) -> Result<(), AdminApiError> {
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(AdminApiError::RelayStatus(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            ))
        }
    }
}
