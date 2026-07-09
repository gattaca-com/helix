use alloy_primitives::hex;
use async_trait::async_trait;
use deadpool_postgres::tokio_postgres;
use helix_database::{
    error::DatabaseError,
    postgres::{
        postgres_db_row_parsing::{FromRow, parse_bytes_to_pubkey_bytes, parse_rows},
        postgres_db_service::PostgresDatabaseService,
    },
};

use crate::models::DemotionResponse;

#[async_trait]
pub trait AdminDatabaseService: Send + Sync {
    async fn get_recent_demotions(
        &self,
        limit: i64,
    ) -> Result<Vec<DemotionResponse>, DatabaseError>;
    async fn get_num_network_validators(&self) -> Result<i64, DatabaseError>;
    async fn get_num_registered_validators(&self) -> Result<i64, DatabaseError>;
    async fn get_num_delivered_payloads(&self) -> Result<i64, DatabaseError>;
}

impl FromRow for DemotionResponse {
    fn from_row(row: &tokio_postgres::Row) -> Result<Self, DatabaseError> {
        Ok(DemotionResponse {
            public_key: parse_bytes_to_pubkey_bytes(row.get::<&str, &[u8]>("public_key"))?,
            demotion_time_ms: row.get::<&str, i64>("demotion_time"),
            reason: row.get::<&str, Option<String>>("reason"),
            block_hash: row.get::<&str, Option<&[u8]>>("block_hash").map(hex::encode_prefixed),
            slot_number: row.get::<&str, Option<i32>>("slot_number"),
        })
    }
}

#[async_trait]
impl AdminDatabaseService for PostgresDatabaseService {
    async fn get_recent_demotions(
        &self,
        limit: i64,
    ) -> Result<Vec<DemotionResponse>, DatabaseError> {
        let query = "
            SELECT public_key, demotion_time, reason, block_hash, slot_number
            FROM demotions
            ORDER BY demotion_time DESC
            LIMIT $1
        ";

        let rows = self.pool.get().await?.query(query, &[&limit]).await?;
        parse_rows(rows)
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
