use alloy_primitives::{Address, B256, U256};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use helix_common::{local_cache::AuctioneerError, simulator::BlockSimError};
use helix_database::error::DatabaseError;
use helix_types::{
    BidValidationError, BlsPublicKey, BlsPublicKeyBytes, ForkName, HydrationError, SigError, Slot,
    ValidationError,
};

#[derive(Debug, thiserror::Error)]
pub enum BuilderApiError {
    #[error("hyper error: {0}")]
    HyperError(#[from] hyper::Error),

    #[error("axum error: {0}")]
    AxumError(#[from] axum::Error),

    #[error("serde decode error: {0}")]
    SerdeDecodeError(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    IOError(#[from] std::io::Error),

    #[error("failed to decode payload")]
    PayloadDecode,

    #[error("ssz serialize error")]
    SszSerializeError,

    #[error("failed to deserialize ssz: {0}")]
    SszDeserializeError(String),

    #[error("payload too large. max size: {max_size}, size: {size}")]
    PayloadTooLarge { max_size: usize, size: usize },

    #[error("submission for past slot. expected: {expected}, got: {got}")]
    SubmissionForPastSlot { expected: Slot, got: Slot },

    #[error("submission for future slot. expected: {expected}, got: {got}")]
    SubmissionForFutureSlot { expected: Slot, got: Slot },

    #[error("submission for wrong slot. expected: {expected}, got: {got}")]
    SubmissionForWrongSlot { expected: Slot, got: Slot },

    #[error("builder blacklisted. pubkey: {pubkey:?}")]
    BuilderBlacklisted { pubkey: BlsPublicKey },

    #[error("incorrect timestamp. got: {got}, expected: {expected}")]
    IncorrectTimestamp { got: u64, expected: u64 },

    #[error("could not find proposer duty for slot")]
    ProposerDutyNotFound,

    #[error("invalid api key")]
    InvalidApiKey,

    #[error("untrusted builder on dehydrated payload")]
    UntrustedBuilderOnDehydratedPayload,

    #[error("payload attributes not yet known")]
    PayloadAttributesNotYetKnown,

    #[error(
        "payload slot mismatches with current payload attributes slot. got: {got}, expected: {expected}"
    )]
    PayloadSlotMismatchWithPayloadAttributes { got: Slot, expected: Slot },

    #[error("{0}")]
    BidValidationError(#[from] BidValidationError),

    #[error("block hash mismatch. message: {message:?}, payload: {payload:?}")]
    BlockHashMismatch { message: B256, payload: B256 },

    #[error("parent hash mismatch. message: {message:?}, payload: {payload:?}")]
    ParentHashMismatch { message: B256, payload: B256 },

    #[error("fee recipient mismatch. got: {got:?}, expected: {expected:?}")]
    FeeRecipientMismatch { got: Address, expected: Address },

    #[error("proposer public key mismatch. got: {got:?}, expected: {expected:?}")]
    ProposerPublicKeyMismatch { got: BlsPublicKeyBytes, expected: BlsPublicKeyBytes },

    #[error("slot mismatch. got: {got}, expected: {expected}")]
    SlotMismatch { got: u64, expected: u64 },

    #[error("zero value block")]
    ZeroValueBlock,

    #[error("missing withdrawls")]
    MissingWithdrawls,

    #[error("invalid withdrawls root")]
    InvalidWithdrawlsRoot,

    #[error("missing withdrawls root")]
    MissingWithdrawlsRoot,

    #[error("withdrawls root mismatch. got: {got:?}, expected: {expected:?}")]
    WithdrawalsRootMismatch { got: B256, expected: B256 },

    #[error("missing transactions")]
    MissingTransactions,

    #[error("missing transactions root")]
    MissingTransactionsRoot,

    #[error("transactions root mismatch. got: {got:?}, expected: {expected:?}")]
    TransactionsRootMismatch { got: B256, expected: B256 },

    // remove this
    #[error("signature verification failed")]
    SignatureVerificationFailed,

    #[error(transparent)]
    SigError(#[from] SigError),

    #[error("payload already delivered")]
    PayloadAlreadyDelivered,

    #[error("delivering payload: bid_slot: {bid_slot}, delivering: {delivering}")]
    DeliveringPayload { bid_slot: u64, delivering: u64 },

    #[error("already processing newer payload")]
    AlreadyProcessingNewerPayload,

    #[error("block validation error: {0:?}")]
    BlockValidationError(#[from] BlockSimError),

    #[error("internal error")]
    InternalError,

    #[error("datastore error: {0}")]
    AuctioneerError(#[from] AuctioneerError),

    #[error("database error: {0}")]
    DatabaseError(#[from] DatabaseError),

    #[error("incorrect prev_randao - got: {got:?}, expected: {expected:?}")]
    PrevRandaoMismatch { got: B256, expected: B256 },

    #[error("block already received: {block_hash:?}")]
    DuplicateBlockHash { block_hash: B256 },

    #[error(
        "not enough optimistic collateral. builder_pub_key: {builder_pub_key:?}. 
        collateral: {collateral:?}, collateral required: {collateral_required:?}"
    )]
    NotEnoughOptimisticCollateral {
        builder_pub_key: BlsPublicKeyBytes,
        collateral: U256,
        collateral_required: U256,
        is_optimistic: bool,
    },

    #[error("builder is not optimistic. builder_pub_key: {builder_pub_key:?}")]
    BuilderNotOptimistic { builder_pub_key: BlsPublicKeyBytes },

    #[error("builder not in proposer's trusted list: {proposer_trusted_builders:?}")]
    BuilderNotInProposersTrustedList { proposer_trusted_builders: Vec<String> },

    #[error("not {fork_name:?} payload")]
    InvalidPayloadType { fork_name: ForkName },

    #[error(transparent)]
    ValidationError(#[from] ValidationError),

    #[error("out of sequence submission: seen: {seen}, this request: {this}")]
    OutOfSequence { seen: u64, this: u64 },

    #[error(transparent)]
    HydrationError(#[from] HydrationError),

    #[error("service unavailable")]
    ServiceUnaivailable,

    #[error("request timeout")]
    RequestTimeout,
}

impl IntoResponse for BuilderApiError {
    fn into_response(self) -> Response {
        let code = match self {
            BuilderApiError::SerdeDecodeError(_) |
            BuilderApiError::IOError(_) |
            BuilderApiError::SszSerializeError |
            BuilderApiError::SszDeserializeError(_) |
            BuilderApiError::HyperError(_) |
            BuilderApiError::AxumError(_) |
            BuilderApiError::PayloadTooLarge { .. } |
            BuilderApiError::SubmissionForPastSlot { .. } |
            BuilderApiError::SubmissionForFutureSlot { .. } |
            BuilderApiError::BuilderBlacklisted { .. } |
            BuilderApiError::IncorrectTimestamp { .. } |
            BuilderApiError::ProposerDutyNotFound |
            BuilderApiError::PayloadSlotMismatchWithPayloadAttributes { .. } |
            BuilderApiError::BlockHashMismatch { .. } |
            BuilderApiError::ParentHashMismatch { .. } |
            BuilderApiError::ProposerPublicKeyMismatch { .. } |
            BuilderApiError::ZeroValueBlock |
            BuilderApiError::SignatureVerificationFailed |
            BuilderApiError::PayloadAlreadyDelivered |
            BuilderApiError::AlreadyProcessingNewerPayload |
            BuilderApiError::FeeRecipientMismatch { .. } |
            BuilderApiError::SlotMismatch { .. } |
            BuilderApiError::MissingWithdrawls |
            BuilderApiError::InvalidWithdrawlsRoot |
            BuilderApiError::MissingWithdrawlsRoot |
            BuilderApiError::WithdrawalsRootMismatch { .. } |
            BuilderApiError::MissingTransactions |
            BuilderApiError::MissingTransactionsRoot |
            BuilderApiError::TransactionsRootMismatch { .. } |
            BuilderApiError::PayloadAttributesNotYetKnown |
            BuilderApiError::PrevRandaoMismatch { .. } |
            BuilderApiError::DuplicateBlockHash { .. } |
            BuilderApiError::NotEnoughOptimisticCollateral { .. } |
            BuilderApiError::BuilderNotOptimistic { .. } |
            BuilderApiError::BuilderNotInProposersTrustedList { .. } |
            // BuilderApiError::PayloadError(_) |
            BuilderApiError::PayloadDecode |
            BuilderApiError::BidValidationError(_) |
            BuilderApiError::ValidationError(_) |
            BuilderApiError::OutOfSequence { .. } |
            BuilderApiError::HydrationError(_) |
            BuilderApiError::SubmissionForWrongSlot { .. } |
            BuilderApiError::SigError(_) | BuilderApiError::DeliveringPayload { .. } => StatusCode::BAD_REQUEST,

            BuilderApiError::InvalidApiKey |
            BuilderApiError::UntrustedBuilderOnDehydratedPayload => StatusCode::UNAUTHORIZED,

            BuilderApiError::InternalError |
            BuilderApiError::AuctioneerError(_) |
            BuilderApiError::DatabaseError(_) => StatusCode::INTERNAL_SERVER_ERROR,

            BuilderApiError::ServiceUnaivailable => StatusCode::SERVICE_UNAVAILABLE,

            BuilderApiError::BlockValidationError(ref err) => match err {
                BlockSimError::Timeout | BlockSimError::SimulationDropped => {
                    StatusCode::REQUEST_TIMEOUT
                }

                _ => StatusCode::BAD_REQUEST,
            },
            BuilderApiError::InvalidPayloadType { .. } => StatusCode::BAD_REQUEST,

            BuilderApiError::RequestTimeout => StatusCode::REQUEST_TIMEOUT
        };

        (code, self.to_string()).into_response()
    }
}
