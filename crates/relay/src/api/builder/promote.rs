use std::sync::Arc;

use axum::{Extension, extract::Query, response::IntoResponse};
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

        let promoted = api.local_cache.promote_builder(&builder_pubkey);

        if !promoted {
            warn!(
                %builder_pubkey,
                "builder already optimistic or not found"
            );

            return (StatusCode::BAD_REQUEST, "builder already optimistic or not found")
                .into_response();
        }

        api.db.db_promote_builder(builder_pubkey);

        let builder_info = api.local_cache.get_builder_info(&builder_pubkey).unwrap_or_default();
        api.alert_manager.send_promotion(
            &format!("✅ *Optimistic promotion successful*\n*Builder:* `{builder_pubkey}`"),
            builder_info.builder_id(),
        );

        (StatusCode::OK, "builder promotion successful").into_response()
    }
}
