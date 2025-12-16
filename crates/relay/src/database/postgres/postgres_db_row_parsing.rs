use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use helix_common::{
    BuilderInfo, Filtering, ProposerInfo, SignedValidatorRegistrationEntry, ValidatorPreferences,
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, data_api::DataAdjustmentsResponse,
        proposer_api::ValidatorRegistrationInfo,
    },
};
use helix_types::{
    BidTrace, BlsPublicKeyBytes, BlsSignatureBytes, SignedValidatorRegistration,
    ValidatorRegistration,
};

use crate::database::{
    BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument, error::DatabaseError,
    postgres::postgres_db_u256_parsing::PostgresNumeric,
};

pub trait FromRow {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized;
}

impl FromRow for DeliveredPayloadDocument {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(DeliveredPayloadDocument {
            bid_trace: BidTrace::from_row(row)?,
            block_number: parse_i32_to_u64(row.get::<&str, i32>("block_number"))?,
            num_txs: parse_i32_to_usize(row.get::<&str, i32>("num_txs"))?,
        })
    }
}

impl FromRow for BidTrace {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(BidTrace {
            slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?,
            parent_hash: parse_bytes_to_hash(row.get::<&str, &[u8]>("parent_hash"))?,
            block_hash: parse_bytes_to_hash(row.get::<&str, &[u8]>("block_hash"))?,
            builder_pubkey: parse_bytes_to_pubkey_bytes(
                row.get::<&str, &[u8]>("builder_public_key"),
            )?,
            proposer_pubkey: parse_bytes_to_pubkey_bytes(
                row.get::<&str, &[u8]>("proposer_public_key"),
            )?,
            proposer_fee_recipient: parse_bytes_to_address(
                row.get::<&str, &[u8]>("proposer_fee_recipient"),
            )?,
            gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
            gas_used: parse_i32_to_u64(row.get::<&str, i32>("gas_used"))?,
            value: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("submission_value")),
        })
    }
}

impl FromRow for BidSubmissionDocument {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(BidSubmissionDocument {
            block_number: parse_i32_to_u64(row.get::<&str, i32>("block_number"))?,
            bid_trace: BidTrace {
                slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?,
                parent_hash: parse_bytes_to_hash(row.get::<&str, &[u8]>("parent_hash"))?,
                block_hash: parse_bytes_to_hash(row.get::<&str, &[u8]>("block_hash"))?,
                builder_pubkey: parse_bytes_to_pubkey_bytes(
                    row.get::<&str, &[u8]>("builder_public_key"),
                )?,
                proposer_pubkey: parse_bytes_to_pubkey_bytes(
                    row.get::<&str, &[u8]>("proposer_public_key"),
                )?,
                proposer_fee_recipient: parse_bytes_to_address(
                    row.get::<&str, &[u8]>("proposer_fee_recipient"),
                )?,
                gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                gas_used: parse_i32_to_u64(row.get::<&str, i32>("gas_used"))?,
                value: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("submission_value")),
            },
            num_txs: parse_i32_to_usize(row.get::<&str, i32>("num_txs"))?,
            timestamp: parse_i64_to_u64(row.get::<&str, i64>("submission_timestamp"))?,
        })
    }
}

impl FromRow for BuilderGetValidatorsResponseEntry {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(BuilderGetValidatorsResponseEntry {
            slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?.into(),
            validator_index: parse_i32_to_usize(row.get::<&str, i32>("validator_index"))? as u64,
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: parse_bytes_to_address(
                            row.get::<&str, &[u8]>("fee_recipient"),
                        )?,
                        gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                        timestamp: parse_i64_to_u64(row.get::<&str, i64>("timestamp"))?,
                        pubkey: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key"))?,
                    },
                    signature: parse_bytes_to_signature_bytes(row.get::<&str, &[u8]>("signature"))?,
                },
                preferences: ValidatorPreferences {
                    //TODO: change to filtering after migration
                    filtering: parse_i16_to_filtering(row.get::<&str, i16>("filtering"))?,
                    trusted_builders: row.get::<&str, Option<Vec<&str>>>("trusted_builders").map(
                        |trusted_builders| {
                            trusted_builders
                                .into_iter()
                                .map(|builder| builder.to_string())
                                .collect()
                        },
                    ),
                    header_delay: row.get::<&str, bool>("header_delay"),
                    delay_ms: row
                        .get::<&str, Option<i64>>("delay_ms")
                        .and_then(|v| parse_i64_to_u64(v).ok()),
                    disable_inclusion_lists: row.get::<&str, bool>("disable_inclusion_lists"),
                },
            },
        })
    }
}

impl FromRow for ValidatorPreferences {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(ValidatorPreferences {
            //TODO: change to filtering after migration
            filtering: parse_i16_to_filtering(row.get::<&str, i16>("filtering"))?,
            trusted_builders: row.get::<&str, Option<Vec<&str>>>("trusted_builders").map(
                |trusted_builders| trusted_builders.into_iter().map(|b| b.to_string()).collect(),
            ),
            header_delay: row.get::<&str, bool>("header_delay"),
            delay_ms: row
                .get::<&str, Option<i64>>("delay_ms")
                .and_then(|v| parse_i64_to_u64(v).ok()),
            disable_inclusion_lists: row.get::<&str, bool>("disable_inclusion_lists"),
        })
    }
}

impl FromRow for BuilderInfoDocument {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(BuilderInfoDocument {
            pub_key: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key"))?,
            builder_info: BuilderInfo::from_row(row)?,
        })
    }
}

impl FromRow for BuilderInfo {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(BuilderInfo {
            collateral: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("collateral")),
            is_optimistic: parse_bool_to_bool(row.get::<&str, bool>("is_optimistic"))?,
            is_optimistic_for_regional_filtering: parse_bool_to_bool(
                row.get::<&str, bool>("is_optimistic_for_regional_filtering"),
            )?,
            builder_id: row.get::<&str, Option<&str>>("builder_id").map(|s| s.to_string()),
            builder_ids: row
                .get::<&str, Option<Vec<&str>>>("builder_ids")
                .map(|ids| ids.into_iter().map(|id| id.to_string()).collect()),
            api_key: row.get::<&str, Option<&str>>("api_key").map(|s| s.to_string()),
        })
    }
}

impl FromRow for SignedValidatorRegistration {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(SignedValidatorRegistration {
            message: ValidatorRegistration {
                fee_recipient: parse_bytes_to_address(row.get::<&str, &[u8]>("fee_recipient"))?,
                gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                timestamp: parse_i64_to_u64(row.get::<&str, i64>("timestamp"))?,
                pubkey: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key"))?,
            },
            signature: parse_bytes_to_signature_bytes(row.get::<&str, &[u8]>("signature"))?,
        })
    }
}

impl FromRow for SignedValidatorRegistrationEntry {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(SignedValidatorRegistrationEntry {
            registration_info: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration::from_row(row)?,
                preferences: ValidatorPreferences {
                    //TODO: change to filtering after migration
                    filtering: parse_i16_to_filtering(row.get::<&str, i16>("filtering"))?,
                    trusted_builders: row.get::<&str, Option<Vec<&str>>>("trusted_builders").map(
                        |trusted_builders| {
                            trusted_builders
                                .into_iter()
                                .map(|builder| builder.to_string())
                                .collect()
                        },
                    ),
                    header_delay: row.get::<&str, bool>("header_delay"),
                    delay_ms: row
                        .get::<&str, Option<i64>>("delay_ms")
                        .and_then(|v| parse_i64_to_u64(v).ok()),
                    disable_inclusion_lists: row.get::<&str, bool>("disable_inclusion_lists"),
                },
            },
            inserted_at: parse_timestamptz_to_u64(
                row.get::<&str, std::time::SystemTime>("inserted_at"),
            )?,
            pool_name: None,
            user_agent: None,
        })
    }
}

impl FromRow for ProposerInfo {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(ProposerInfo {
            name: row.get::<&str, &str>("name").to_string(),
            pubkey: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("pub_key"))?,
        })
    }
}

impl FromRow for DataAdjustmentsResponse {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(DataAdjustmentsResponse {
            builder_pubkey: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("builder_pubkey"))?,
            block_number: parse_i64_to_u64(row.get::<&str, i64>("block_number"))?,
            delta: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("delta")),
            submitted_block_hash: parse_bytes_to_hash(
                row.get::<&str, &[u8]>("submitted_block_hash"),
            )?,
            submitted_received_at: parse_timestamptz_to_datetime(
                row.get::<&str, std::time::SystemTime>("submitted_received_at"),
            ),
            submitted_value: parse_numeric_to_u256(
                row.get::<&str, PostgresNumeric>("submitted_value"),
            ),
            adjusted_block_hash: parse_bytes_to_hash(
                row.get::<&str, &[u8]>("adjusted_block_hash"),
            )?,
            adjusted_value: parse_numeric_to_u256(
                row.get::<&str, PostgresNumeric>("adjusted_value"),
            ),
        })
    }
}

pub fn parse_timestamptz_to_u64(timestamp: std::time::SystemTime) -> Result<u64, DatabaseError> {
    timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
        .map(|duration| duration.as_secs())
}

pub fn parse_timestamptz_to_datetime(timestamp: std::time::SystemTime) -> DateTime<Utc> {
    timestamp.into()
}

pub fn parse_bool_to_bool(value: bool) -> Result<bool, DatabaseError> {
    Ok(value)
}

pub fn parse_i16_to_filtering(value: i16) -> Result<Filtering, DatabaseError> {
    match value {
        1 => Ok(Filtering::Regional),
        0 => Ok(Filtering::Global),
        _ => Err(DatabaseError::GeneralError),
    }
}

pub fn parse_i32_to_usize(value: i32) -> Result<usize, DatabaseError> {
    usize::try_from(value).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_i32_to_u64(value: i32) -> Result<u64, DatabaseError> {
    u64::try_from(value).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_i64_to_u64(value: i64) -> Result<u64, DatabaseError> {
    u64::try_from(value).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_bytes_to_hash(hash: &[u8]) -> Result<B256, DatabaseError> {
    B256::try_from(hash).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_bytes_to_address(hash: &[u8]) -> Result<Address, DatabaseError> {
    Address::try_from(hash).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_bytes_to_pubkey_bytes(pubkey: &[u8]) -> Result<BlsPublicKeyBytes, DatabaseError> {
    BlsPublicKeyBytes::try_from(pubkey).map_err(|_| DatabaseError::InvalidBlsBytes)
}

pub fn parse_bytes_to_signature_bytes(
    signature: &[u8],
) -> Result<BlsSignatureBytes, DatabaseError> {
    BlsSignatureBytes::try_from(signature).map_err(|_| DatabaseError::InvalidBlsBytes)
}

pub fn parse_numeric_to_u256(value: PostgresNumeric) -> U256 {
    U256::from(value.0)
}

pub fn parse_rows<T: FromRow>(rows: Vec<tokio_postgres::Row>) -> Result<Vec<T>, DatabaseError> {
    rows.iter().map(|row| T::from_row(row)).collect()
}

pub fn parse_row<T: FromRow>(row: &tokio_postgres::Row) -> Result<T, DatabaseError> {
    T::from_row(row)
}
