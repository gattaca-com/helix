use std::{collections::HashSet, sync::Arc};

use alloy_primitives::U256;
use axum::{
    Json,
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
};
use helix_common::{
    ValidatorPreferences,
    api::data_api::{
        BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
        ReceivedBlocksResponse,
    },
};
use helix_database::{
    error::DatabaseError, postgres::postgres_db_service::PostgresDatabaseService,
};
use helix_types::BlsPublicKeyBytes;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    db::AdminDatabaseService,
    error::AdminApiError,
    models::{BuilderResponse, CreateBuilderRequest, OverviewResponse, UpdateCollateralRequest},
    relay_client::RelayAdminClient,
};

const MAX_PAYLOADS_LIMIT: u64 = 200;
const DEFAULT_PAYLOADS_LIMIT: u64 = 50;
const MAX_DEMOTIONS_LIMIT: i64 = 500;
const DEFAULT_DEMOTIONS_LIMIT: i64 = 100;

/// Minimum collateral (in wei) required for a builder to be eligible for optimistic processing.
const MIN_OPTIMISTIC_COLLATERAL_WEI: u64 = 1_000_000_000_000_000_000;

pub async fn overview(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    let adjustments_enabled = db.check_adjustments_enabled().await?;
    let builders_pending_promotion = db.count_builders_pending_promotion().await?;

    let (kill_switch_enabled, block_merging_enabled) = match relay.status().await {
        Ok(status) => (Some(status.kill_switch_enabled), Some(status.block_merging_enabled)),
        Err(err) => {
            warn!(%err, "relay admin API unreachable, omitting kill switch/block merging state");
            (None, None)
        }
    };

    Ok(Json(OverviewResponse {
        adjustments_enabled,
        kill_switch_enabled,
        block_merging_enabled,
        builders_pending_promotion,
    }))
}

pub async fn builders(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
) -> Result<impl IntoResponse, AdminApiError> {
    let builders = db.get_all_builder_infos().await?;
    Ok(Json(builders.into_iter().map(BuilderResponse::from).collect::<Vec<_>>()))
}

pub async fn create_builder(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Json(req): Json<CreateBuilderRequest>,
) -> Result<impl IntoResponse, AdminApiError> {
    if req.is_optimistic && req.collateral <= U256::from(MIN_OPTIMISTIC_COLLATERAL_WEI) {
        return Err(AdminApiError::InsufficientCollateralForOptimistic);
    }
    db.create_builder_info(&req.pub_key, req.builder_id, req.collateral, req.is_optimistic).await?;
    info!(pubkey = %req.pub_key, "builder pubkey added via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_builder_collateral(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
    Json(req): Json<UpdateCollateralRequest>,
) -> Result<impl IntoResponse, AdminApiError> {
    db.update_builder_collateral(&pubkey, req.collateral).await?;
    info!(%pubkey, "builder collateral updated via admin website");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DemotionsParams {
    pub limit: Option<i64>,
}

pub async fn demotions(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Query(params): Query<DemotionsParams>,
) -> Result<impl IntoResponse, AdminApiError> {
    let limit = params.limit.unwrap_or(DEFAULT_DEMOTIONS_LIMIT).clamp(1, MAX_DEMOTIONS_LIMIT);
    let demotions = db.get_recent_demotions(limit).await?;
    Ok(Json(demotions))
}

pub async fn payloads(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Extension(validator_preferences): Extension<Arc<ValidatorPreferences>>,
    Query(mut params): Query<ProposerPayloadDeliveredParams>,
) -> Result<impl IntoResponse, AdminApiError> {
    if params.slot.is_some() && params.cursor.is_some() {
        return Err(AdminApiError::SlotAndCursor);
    }
    if params.limit.is_some_and(|limit| limit > MAX_PAYLOADS_LIMIT) {
        return Err(AdminApiError::LimitReached { limit: MAX_PAYLOADS_LIMIT });
    }
    if params.limit.is_none() {
        params.limit = Some(DEFAULT_PAYLOADS_LIMIT);
    }

    let result = db.get_delivered_payloads(&(&params).into(), validator_preferences).await?;

    let mut seen = HashSet::with_capacity(result.len());
    let response = result
        .into_iter()
        .filter(|b| seen.insert(b.bid_trace.block_hash))
        .map(|b| b.into())
        .collect::<Vec<DeliveredPayloadsResponse>>();

    Ok(Json(response))
}

pub async fn bids(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Extension(validator_preferences): Extension<Arc<ValidatorPreferences>>,
    Query(params): Query<BuilderBlocksReceivedParams>,
) -> Result<impl IntoResponse, AdminApiError> {
    if params.slot.is_none() {
        return Err(AdminApiError::MissingSlot);
    }

    let result = db.get_bids(&(&params).into(), validator_preferences).await?;

    let mut seen = HashSet::with_capacity(result.len());
    let response = result
        .into_iter()
        .filter(|b| seen.insert(b.bid_trace.block_hash))
        .map(|b| b.into())
        .collect::<Vec<ReceivedBlocksResponse>>();

    Ok(Json(response))
}

pub async fn validator_registration(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
) -> Result<impl IntoResponse, AdminApiError> {
    match db.get_validator_registration(&pubkey).await {
        Ok(entry) => Ok(Json(entry.registration_info.registration)),
        Err(DatabaseError::ValidatorRegistrationNotFound) => {
            Err(AdminApiError::ValidatorRegistrationNotFound { pubkey })
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn trusted_proposers(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
) -> Result<impl IntoResponse, AdminApiError> {
    let proposers = db.get_trusted_proposers().await?;
    Ok(Json(proposers))
}

pub async fn adjustments_status(
    Extension(db): Extension<Arc<PostgresDatabaseService>>,
) -> Result<impl IntoResponse, AdminApiError> {
    let enabled = db.check_adjustments_enabled().await?;
    Ok(Json(serde_json::json!({ "enabled": enabled })))
}

// Action handlers, proxied to the relay's admin API.

pub async fn enable_kill_switch(
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.set_kill_switch(true).await?;
    info!("kill switch enabled via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable_kill_switch(
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.set_kill_switch(false).await?;
    info!("kill switch disabled via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn enable_block_merging(
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.set_block_merging(true).await?;
    info!("block merging enabled via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable_block_merging(
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.set_block_merging(false).await?;
    info!("block merging disabled via admin website");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct DemoteBuilderRequest {
    pub reason: Option<String>,
}

pub async fn demote_builder(
    Extension(relay): Extension<RelayAdminClient>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
    body: Option<Json<DemoteBuilderRequest>>,
) -> Result<impl IntoResponse, AdminApiError> {
    let reason = body.and_then(|Json(req)| req.reason);
    relay.demote_builder(&pubkey, reason).await?;
    info!(%pubkey, "builder demoted via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn promote_builder(
    Extension(relay): Extension<RelayAdminClient>,
    Path(pubkey): Path<BlsPublicKeyBytes>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.promote_builder(&pubkey).await?;
    info!(%pubkey, "builder promoted via admin website");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn disable_adjustments(
    Extension(relay): Extension<RelayAdminClient>,
) -> Result<impl IntoResponse, AdminApiError> {
    relay.disable_adjustments().await?;
    info!("adjustments disabled via admin website");
    Ok(StatusCode::NO_CONTENT)
}
