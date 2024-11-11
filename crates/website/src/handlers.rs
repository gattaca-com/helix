use crate::state::AppState;
use askama::Template;
use axum::{extract::Query, extract::State, response::Html};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info};

pub async fn index(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Html<String>, axum::http::StatusCode> {
    info!("Handling website request");
    let order_by = params.get("order_by").map(|s| s.as_str());

    // Await the read lock on cached_templates
    let cached_templates = state.cached_templates.read().await;

    let template = match order_by {
        Some("-value") => &cached_templates.by_value_desc,
        Some("value") => &cached_templates.by_value_asc,
        _ => &cached_templates.default,
    };

    template.render().map(Html).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}
