pub mod api;
pub mod error;
pub mod tests;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub use api::*;
use helix_common::api::data_api::{
    BuilderBlocksReceivedParams, DeliveredPayloadsResponse, ProposerPayloadDeliveredParams,
};
use moka::Expiry;
use tracing::info;

pub struct DataApiStats {}

#[derive(Debug, Clone, Default)]
pub struct ProposerPayloadDeliveredStats {
    pub total: Arc<AtomicUsize>,
    pub cache_hit: Arc<AtomicUsize>,
    pub with_slot: Arc<AtomicUsize>,
    pub with_cursor: Arc<AtomicUsize>,
    pub with_limit: Arc<AtomicUsize>,
    pub with_block_hash: Arc<AtomicUsize>,
    pub with_block_number: Arc<AtomicUsize>,
    pub with_proposer_pubkey: Arc<AtomicUsize>,
    pub with_builder_pubkey: Arc<AtomicUsize>,
    pub with_order_by: Arc<AtomicUsize>,
}

impl ProposerPayloadDeliveredStats {
    pub fn record_total(&self, query: &ProposerPayloadDeliveredParams) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if query.slot.is_some() {
            self.with_slot.fetch_add(1, Ordering::Relaxed);
        }
        if query.cursor.is_some() {
            self.with_cursor.fetch_add(1, Ordering::Relaxed);
        }
        if query.limit.is_some() {
            self.with_limit.fetch_add(1, Ordering::Relaxed);
        }
        if query.block_hash.is_some() {
            self.with_block_hash.fetch_add(1, Ordering::Relaxed);
        }
        if query.block_number.is_some() {
            self.with_block_number.fetch_add(1, Ordering::Relaxed);
        }
        if query.proposer_pubkey.is_some() {
            self.with_proposer_pubkey.fetch_add(1, Ordering::Relaxed);
        }
        if query.builder_pubkey.is_some() {
            self.with_builder_pubkey.fetch_add(1, Ordering::Relaxed);
        }
        if query.order_by.is_some() {
            self.with_order_by.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_cache_hit(&self) {
        self.cache_hit.fetch_add(1, Ordering::Relaxed);
    }

    pub fn maybe_log_reset(&self) {
        let total = self.total.swap(0, Ordering::Relaxed);
        if total > 0 {
            let cache_hit = self.cache_hit.swap(0, Ordering::Relaxed);
            let with_slot = self.with_slot.swap(0, Ordering::Relaxed);
            let with_cursor = self.with_cursor.swap(0, Ordering::Relaxed);
            let with_limit = self.with_limit.swap(0, Ordering::Relaxed);
            let with_block_hash = self.with_block_hash.swap(0, Ordering::Relaxed);
            let with_block_number = self.with_block_number.swap(0, Ordering::Relaxed);
            let with_proposer_pubkey = self.with_proposer_pubkey.swap(0, Ordering::Relaxed);
            let with_builder_pubkey = self.with_builder_pubkey.swap(0, Ordering::Relaxed);
            let with_order_by = self.with_order_by.swap(0, Ordering::Relaxed);

            info!(
                total,
                cache_hit,
                with_slot,
                with_cursor,
                with_limit,
                with_block_hash,
                with_block_number,
                with_proposer_pubkey,
                with_builder_pubkey,
                with_order_by,
                "proposer payload delivered stats"
            );
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BuilderBlocksReceivedStats {
    pub total: Arc<AtomicUsize>,
    pub cache_hit: Arc<AtomicUsize>,
    pub with_slot: Arc<AtomicUsize>,
    pub with_block_hash: Arc<AtomicUsize>,
    pub with_block_number: Arc<AtomicUsize>,
    pub with_builder_pubkey: Arc<AtomicUsize>,
    pub with_limit: Arc<AtomicUsize>,
}

impl BuilderBlocksReceivedStats {
    pub fn record_total(&self, query: &BuilderBlocksReceivedParams) {
        self.total.fetch_add(1, Ordering::Relaxed);
        if query.slot.is_some() {
            self.with_slot.fetch_add(1, Ordering::Relaxed);
        }
        if query.block_hash.is_some() {
            self.with_block_hash.fetch_add(1, Ordering::Relaxed);
        }
        if query.block_number.is_some() {
            self.with_block_number.fetch_add(1, Ordering::Relaxed);
        }
        if query.builder_pubkey.is_some() {
            self.with_builder_pubkey.fetch_add(1, Ordering::Relaxed);
        }
        if query.limit.is_some() {
            self.with_limit.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_cache_hit(&self) {
        self.cache_hit.fetch_add(1, Ordering::Relaxed);
    }

    pub fn maybe_log_reset(&self) {
        let total = self.total.swap(0, Ordering::Relaxed);
        if total > 0 {
            let cache_hit = self.cache_hit.swap(0, Ordering::Relaxed);
            let with_slot = self.with_slot.swap(0, Ordering::Relaxed);
            let with_block_hash = self.with_block_hash.swap(0, Ordering::Relaxed);
            let with_block_number = self.with_block_number.swap(0, Ordering::Relaxed);
            let with_builder_pubkey = self.with_builder_pubkey.swap(0, Ordering::Relaxed);

            info!(
                total,
                cache_hit,
                with_slot,
                with_block_hash,
                with_block_number,
                with_builder_pubkey,
                "builder blocks received stats"
            );
        }
    }
}

pub struct SelectiveExpiry;

impl Expiry<ProposerPayloadDeliveredParams, Vec<DeliveredPayloadsResponse>> for SelectiveExpiry {
    fn expire_after_create(
        &self,
        key: &ProposerPayloadDeliveredParams,
        _val: &Vec<DeliveredPayloadsResponse>,
        _now: Instant,
    ) -> Option<Duration> {
        if key.slot.is_none() &&
            key.cursor.is_none() &&
            key.block_hash.is_none() &&
            key.block_number.is_none() &&
            key.proposer_pubkey.is_none() &&
            key.builder_pubkey.is_none()
        {
            Some(Duration::from_secs(12))
        } else {
            None
        }
    }
}
