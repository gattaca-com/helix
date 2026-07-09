use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use helix_database::error::DatabaseError;
use helix_types::BlsPublicKeyBytes;

#[derive(Debug, thiserror::Error)]
pub enum AdminApiError {
    #[error("maximum limit is {limit}")]
    LimitReached { limit: u64 },
    #[error("cannot specify both slot and cursor")]
    SlotAndCursor,
    #[error("need to query for a specific slot")]
    MissingSlot,
    #[error("no registration found for validator {pubkey}")]
    ValidatorRegistrationNotFound { pubkey: BlsPublicKeyBytes },
    #[error("database error")]
    Database(#[from] DatabaseError),
    #[error("relay admin API unreachable: {0}")]
    RelayUnreachable(#[from] reqwest::Error),
    #[error("relay admin API returned {0}")]
    RelayStatus(StatusCode),
}

impl IntoResponse for AdminApiError {
    fn into_response(self) -> Response {
        let code = match &self {
            AdminApiError::LimitReached { .. } |
            AdminApiError::SlotAndCursor |
            AdminApiError::MissingSlot => StatusCode::BAD_REQUEST,
            AdminApiError::ValidatorRegistrationNotFound { .. } => StatusCode::NOT_FOUND,
            AdminApiError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminApiError::RelayUnreachable(_) => StatusCode::BAD_GATEWAY,
            AdminApiError::RelayStatus(status) if status == &StatusCode::NOT_FOUND => {
                StatusCode::NOT_FOUND
            }
            AdminApiError::RelayStatus(_) => StatusCode::BAD_GATEWAY,
        };

        (code, self.to_string()).into_response()
    }
}
