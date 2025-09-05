use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use helix_types::BlsPublicKeyBytes;

#[derive(Debug, thiserror::Error)]
pub enum DataApiError {
    #[error("cannot specify both slot and cursor")]
    SlotAndCursor,
    #[error("need to query for specific slot or block_hash or block_number or builder_pubkey")]
    MissingFilter,
    #[error("maximum limit is {limit}")]
    LimitReached { limit: u64 },
    #[error("no registration found for validator {pubkey}")]
    ValidatorRegistrationNotFound { pubkey: BlsPublicKeyBytes },
    #[error("internal server error")]
    InternalServerError,
}

impl IntoResponse for DataApiError {
    fn into_response(self) -> Response {
        let code = match self {
            DataApiError::SlotAndCursor |
            DataApiError::MissingFilter |
            DataApiError::LimitReached { .. } |
            DataApiError::ValidatorRegistrationNotFound { .. } => StatusCode::BAD_REQUEST,

            DataApiError::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (code, self.to_string()).into_response()
    }
}
