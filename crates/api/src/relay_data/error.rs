use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(Debug, thiserror::Error)]
pub enum DataApiError {
    #[error("cannot specify both slot and cursor")]
    SlotAndCursor,
    #[error("need to query for specific slot or block_hash or block_number or builder_pubkey")]
    MissingFilter,
    #[error("maximum limit is {limit}")]
    LimitReached { limit: u64 },
    #[error("internal server error")]
    InternalServerError,
}

impl IntoResponse for DataApiError {
    fn into_response(self) -> Response {
        match self {
            DataApiError::SlotAndCursor => {
                (StatusCode::BAD_REQUEST, "cannot specify both slot and cursor").into_response()
            }
            DataApiError::MissingFilter => (
                StatusCode::BAD_REQUEST,
                "need to query for specific slot or block_hash or block_number or builder_pubkey",
            )
                .into_response(),
            DataApiError::LimitReached { limit } => {
                (StatusCode::BAD_REQUEST, format!("maximum limit is {limit}")).into_response()
            }
            DataApiError::InternalServerError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}
