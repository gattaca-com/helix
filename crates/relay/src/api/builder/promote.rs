use std::sync::Arc;

use axum::{Extension, extract::Query, response::IntoResponse};
use helix_common::utils::utcnow_ms;
use helix_types::{OperatorMessage, Promotion};
use http::StatusCode;
use serde::Deserialize;
use tracing::warn;

use super::api::BuilderApi;
use crate::Api;

#[derive(Debug, Deserialize)]
pub struct PromoteBuilderParams {
    pub token: String,
}

impl<A: Api> BuilderApi<A> {
    #[tracing::instrument(skip_all, fields(builder_pubkey = tracing::field::Empty))]
    pub async fn promote_builder(
        Extension(api): Extension<Arc<BuilderApi<A>>>,
        Query(params): Query<PromoteBuilderParams>,
    ) -> impl IntoResponse {
        let Some(builder_pubkey) = api.alert_manager.consume_promotion_token(&params.token) else {
            warn!("invalid or expired promotion token received");
            return (StatusCode::UNAUTHORIZED, "invalid or expired promotion token").into_response();
        };

        tracing::Span::current().record("builder_pubkey", tracing::field::display(builder_pubkey));

        // From `Follow` the flag is advisory: pool state decides status, and a promotion whose
        // purpose is to clear reservations must proceed even when the flag is already set.
        let applies = api.local_cache.promotion_mode().applies();
        let promoted = api.local_cache.promote_builder(&builder_pubkey);

        if !promoted && !applies {
            warn!(
                %builder_pubkey,
                "builder already optimistic or not found"
            );

            return (StatusCode::BAD_REQUEST, "builder already optimistic or not found")
                .into_response();
        }

        let builder_info = api.local_cache.get_builder_info(&builder_pubkey).unwrap_or_default();
        let collateral_id = builder_info.builder_id.clone().unwrap_or_default();
        let ts_ms = utcnow_ms();
        let slot = api.curr_slot_info.head_slot().as_u64();
        api.db.db_promote_builder(builder_pubkey, collateral_id.clone(), ts_ms, slot);

        api.alert_manager.send_promotion(
            &format!("✅ *Optimistic promotion successful*\n*Builder:* `{builder_pubkey}`"),
            builder_info.builder_id(),
        );

        if let Some(operator_api) = api.operator_api.as_ref() &&
            let Err(e) = operator_api
                .send(OperatorMessage::Promotion(Promotion {
                    ts_ms,
                    slot,
                    collateral_id: collateral_id.into_bytes(),
                    builder_pubkey,
                }))
                .await
        {
            tracing::error!(?e, "failed to send operator promote message for {:?}", builder_pubkey);
        }

        (StatusCode::OK, "builder promotion successful").into_response()
    }
}
