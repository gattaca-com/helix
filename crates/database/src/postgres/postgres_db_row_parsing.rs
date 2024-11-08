use ethereum_consensus::{
    builder::{SignedValidatorRegistration, ValidatorRegistration},
    primitives::{BlsPublicKey, BlsSignature, U256},
};
use helix_common::{
    api::{
        builder_api::BuilderGetValidatorsResponseEntry, proposer_api::ValidatorRegistrationInfo,
    },
    bellatrix::{ByteList, ByteVector, List},
    bid_submission::BidTrace,
    BuilderInfo, Filtering, GetPayloadTrace, ProposerInfo, SignedValidatorRegistrationEntry,
    ValidatorPreferences,
};
use thiserror::Error;

use crate::{
    error::DatabaseError, postgres::postgres_db_u256_parsing::PostgresNumeric,
    BidSubmissionDocument, BuilderInfoDocument, DeliveredPayloadDocument,
};

#[derive(Debug, Error)]
pub enum RowParsingError {
    #[error("Parsing error: {0}")]
    General(#[from] Box<dyn std::error::Error>),
}

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
            parent_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("parent_hash"))?,
            block_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("block_hash"))?,
            builder_public_key: parse_bytes_to_pubkey(
                row.get::<&str, &[u8]>("builder_public_key"),
            )?,
            proposer_public_key: parse_bytes_to_pubkey(
                row.get::<&str, &[u8]>("proposer_public_key"),
            )?,
            proposer_fee_recipient: parse_bytes_to_hash::<20>(
                row.get::<&str, &[u8]>("proposer_fee_recipient"),
            )?,
            gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
            gas_used: parse_i32_to_u64(row.get::<&str, i32>("gas_used"))?,
            value: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("submission_value")),
        })
    }
}

impl FromRow for GetPayloadTrace {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(GetPayloadTrace {
            receive: parse_i64_to_u64(row.get::<&str, i64>("receive"))?,
            proposer_index_validated: parse_i64_to_u64(
                row.get::<&str, i64>("proposer_index_validated"),
            )?,
            signature_validated: parse_i64_to_u64(row.get::<&str, i64>("signature_validated"))?,
            payload_fetched: parse_i64_to_u64(row.get::<&str, i64>("payload_fetched"))?,
            validation_complete: parse_i64_to_u64(row.get::<&str, i64>("validation_complete"))?,
            beacon_client_broadcast: parse_i64_to_u64(
                row.get::<&str, i64>("beacon_client_broadcast"),
            )?,
            broadcaster_block_broadcast: parse_i64_to_u64(
                row.get::<&str, i64>("broadcaster_block_broadcast"),
            )?,
            on_deliver_payload: parse_i64_to_u64(row.get::<&str, i64>("on_deliver_payload"))?,
        })
    }
}

impl<
        const BYTES_PER_LOGS_BLOOM: usize,
        const MAX_EXTRA_DATA_BYTES: usize,
        const MAX_BYTES_PER_TRANSACTION: usize,
        const MAX_TRANSACTIONS_PER_PAYLOAD: usize,
    > FromRow
    for ethereum_consensus::bellatrix::ExecutionPayload<
        BYTES_PER_LOGS_BLOOM,
        MAX_EXTRA_DATA_BYTES,
        MAX_BYTES_PER_TRANSACTION,
        MAX_TRANSACTIONS_PER_PAYLOAD,
    >
{
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(ethereum_consensus::bellatrix::ExecutionPayload {
            parent_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("payload_parent_hash"))?,
            fee_recipient: parse_bytes_to_hash::<20>(
                row.get::<&str, &[u8]>("payload_fee_recipient"),
            )?,
            state_root: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("payload_state_root"))?,
            receipts_root: parse_bytes_to_hash::<32>(
                row.get::<&str, &[u8]>("payload_receipts_root"),
            )?,
            logs_bloom: parse_bytes_to_hash::<BYTES_PER_LOGS_BLOOM>(
                row.get::<&str, &[u8]>("payload_logs_bloom"),
            )?,
            prev_randao: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("payload_prev_randao"))?,
            block_number: parse_i32_to_u64(row.get::<&str, i32>("payload_block_number"))?,
            gas_limit: parse_i32_to_u64(row.get::<&str, i32>("payload_gas_limit"))?,
            gas_used: parse_i32_to_u64(row.get::<&str, i32>("payload_gas_used"))?,
            timestamp: parse_i64_to_u64(row.get::<&str, i64>("payload_timestamp"))?,
            extra_data: parse_bytes_to_bytelist::<MAX_EXTRA_DATA_BYTES>(
                row.get::<&str, &[u8]>("payload_extra_data"),
            )?,
            base_fee_per_gas: parse_numeric_to_u256(
                row.get::<&str, PostgresNumeric>("payload_base_fee_per_gas"),
            ),
            block_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("payload_block_hash"))?,
            transactions: parse_vec_bytes_to_list_bytelist::<
                MAX_BYTES_PER_TRANSACTION,
                MAX_TRANSACTIONS_PER_PAYLOAD,
            >(row.get::<&str, Vec<Vec<u8>>>("txs"))?,
        })
    }
}

impl FromRow for BidSubmissionDocument {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(BidSubmissionDocument {
            block_number: parse_i32_to_u64(row.get::<&str, i32>("block_number"))?,
            bid_trace: BidTrace {
                slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?,
                parent_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("parent_hash"))?,
                block_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("block_hash"))?,
                builder_public_key: parse_bytes_to_pubkey(
                    row.get::<&str, &[u8]>("builder_public_key"),
                )?,
                proposer_public_key: parse_bytes_to_pubkey(
                    row.get::<&str, &[u8]>("proposer_public_key"),
                )?,
                proposer_fee_recipient: parse_bytes_to_hash::<20>(
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
            slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?,
            validator_index: parse_i32_to_usize(row.get::<&str, i32>("validator_index"))?,
            entry: ValidatorRegistrationInfo {
                registration: SignedValidatorRegistration {
                    message: ValidatorRegistration {
                        fee_recipient: parse_bytes_to_hash::<20>(
                            row.get::<&str, &[u8]>("fee_recipient"),
                        )?,
                        gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                        timestamp: parse_i64_to_u64(row.get::<&str, i64>("timestamp"))?,
                        public_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("public_key"))?,
                    },
                    signature: parse_bytes_to_signature(row.get::<&str, &[u8]>("signature"))?,
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
                    gossip_blobs: row.get::<&str, bool>("gossip_blobs"),
                },
            },
        })
    }
}

impl FromRow for BuilderInfoDocument {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError>
    where
        Self: Sized,
    {
        Ok(BuilderInfoDocument {
            pub_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("public_key"))?,
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
            builder_id: row.get::<&str, Option<&str>>("builder_id").map(|s| s.to_string()),
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
                fee_recipient: parse_bytes_to_hash::<20>(row.get::<&str, &[u8]>("fee_recipient"))?,
                gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                timestamp: parse_i64_to_u64(row.get::<&str, i64>("timestamp"))?,
                public_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("public_key"))?,
            },
            signature: parse_bytes_to_signature(row.get::<&str, &[u8]>("signature"))?,
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
                    gossip_blobs: row.get::<&str, bool>("gossip_blobs"),
                },
            },
            inserted_at: parse_timestamptz_to_u64(
                row.get::<&str, std::time::SystemTime>("inserted_at"),
            )?,
            pool_name: None,
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
            pub_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("pub_key"))?,
        })
    }
}

pub fn parse_timestamptz_to_u64(timestamp: std::time::SystemTime) -> Result<u64, DatabaseError> {
    timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
        .map(|duration| duration.as_secs())
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

pub fn parse_bytes_to_hash<const N: usize>(hash: &[u8]) -> Result<ByteVector<N>, DatabaseError> {
    ByteVector::try_from(hash).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_bytes_to_pubkey(pubkey: &[u8]) -> Result<BlsPublicKey, DatabaseError> {
    BlsPublicKey::try_from(pubkey).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_bytes_to_signature(signature: &[u8]) -> Result<BlsSignature, DatabaseError> {
    BlsSignature::try_from(signature).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
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

pub fn parse_bytes_to_bytelist<const N: usize>(bytes: &[u8]) -> Result<ByteList<N>, DatabaseError> {
    ByteList::try_from(bytes).map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
}

pub fn parse_vec_bytes_to_list_bytelist<const N: usize, const M: usize>(
    bytes_vec: Vec<Vec<u8>>,
) -> Result<List<ByteList<N>, M>, DatabaseError> {
    let tmp: Result<Vec<ByteList<N>>, DatabaseError> = bytes_vec
        .iter()
        .map(|f| {
            ByteList::try_from(f.as_slice())
                .map_err(|e| DatabaseError::RowParsingError(Box::new(e)))
        })
        .collect();
    tmp.and_then(|list| List::try_from(list).map_err(|_| DatabaseError::GeneralError))
}
