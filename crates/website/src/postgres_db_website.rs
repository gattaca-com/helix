use deadpool_postgres::tokio_postgres;
use async_trait::async_trait;
use helix_database::error::DatabaseError;
use crate::models::DeliveredPayload;
use helix_common::bid_submission::BidTrace;
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_database::postgres::postgres_db_u256_parsing::{PostgresNumeric};
use helix_database::postgres::postgres_db_row_parsing::{parse_bytes_to_hash, parse_bytes_to_pubkey, parse_numeric_to_u256, FromRow, parse_rows, parse_i32_to_u64, parse_i32_to_usize};

#[async_trait]
pub trait WebsiteDatabaseService: Send + Sync {
    async fn get_recent_delivered_payloads(&self, limit: i64) -> Result<Vec<DeliveredPayload>, DatabaseError>;
    async fn get_num_network_validators(&self) -> Result<i64, DatabaseError>;
    async fn get_num_registered_validators(&self) -> Result<i64, DatabaseError>;
    async fn get_num_delivered_payloads(&self) -> Result<i64, DatabaseError>;
}


impl FromRow for DeliveredPayload {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(DeliveredPayload {
            bid_trace: BidTrace {
                slot: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?,
                parent_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("parent_hash"))?,
                block_hash: parse_bytes_to_hash::<32>(row.get::<&str, &[u8]>("block_hash"))?,
                builder_public_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("builder_pubkey"))?,
                proposer_public_key: parse_bytes_to_pubkey(row.get::<&str, &[u8]>("proposer_pubkey"))?,
                proposer_fee_recipient: parse_bytes_to_hash::<20>(row.get::<&str, &[u8]>("proposer_fee_recipient"))?,
                gas_limit: parse_i32_to_u64(row.get::<&str, i32>("gas_limit"))?,
                gas_used: parse_i32_to_u64(row.get::<&str, i32>("gas_used"))?,
                value: parse_numeric_to_u256(row.get::<&str, PostgresNumeric>("value")),
            },
            block_number: parse_i32_to_u64(row.get::<&str, i32>("block_number"))?,
            num_txs: parse_i32_to_usize(row.get::<&str, i32>("num_txs"))?,
            num_blobs: parse_i32_to_usize(row.get::<&str, i32>("num_blobs"))?,
            blob_gas_used: parse_i32_to_u64(row.get::<&str, i32>("blob_gas_used"))?,
            excess_blob_gas: parse_i32_to_u64(row.get::<&str, i32>("excess_blob_gas"))?,
            epoch: parse_i32_to_u64(row.get::<&str, i32>("slot_number"))?/32 //Calculate directly
        })
    }
}

#[async_trait]
impl WebsiteDatabaseService for PostgresDatabaseService {
    async fn get_recent_delivered_payloads(&self, limit: i64) -> Result<Vec<DeliveredPayload>, DatabaseError> {
        let query = "
        SELECT
            block_submission.slot_number,
            block_submission.parent_hash,
            block_submission.block_hash,
            block_submission.builder_pubkey,
            block_submission.proposer_pubkey,
            block_submission.proposer_fee_recipient,
            block_submission.gas_limit,
            block_submission.gas_used,
            block_submission.value,
            block_submission.num_txs,
            block_submission.block_number,
            block_submission.num_blobs,
            block_submission.blob_gas_used,
            block_submission.excess_blob_gas
        FROM
            delivered_payload
        INNER JOIN
            block_submission ON block_submission.block_hash = delivered_payload.block_hash
        ORDER BY block_submission.slot_number DESC
        LIMIT $1
        ";

        let rows = self.pool.get().await?.query(query, &[&limit]).await?;
        let payloads = parse_rows(rows)?;
        Ok(payloads)
    }

    async fn get_num_network_validators(&self) -> Result<i64, DatabaseError> {
        let client = self.pool.get().await?;
        let row = client.query_one("SELECT COUNT(*) FROM known_validators", &[]).await?;
        Ok(row.get::<usize, i64>(0))
    }

    async fn get_num_registered_validators(&self) -> Result<i64, DatabaseError> {
        let client = self.pool.get().await?;
        let row = client.query_one("SELECT COUNT(*) FROM validator_registrations", &[]).await?;
        Ok(row.get::<usize, i64>(0))
    }


    async fn get_num_delivered_payloads(&self) -> Result<i64, DatabaseError> {
        let client = self.pool.get().await?;
        let row = client.query_one("SELECT COUNT(*) FROM delivered_payload", &[]).await?;
        Ok(row.get::<usize, i64>(0))
    }
}