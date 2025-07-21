use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy_primitives::{map::foldhash::HashSet, B256};
use helix_common::{
    bid_submission::{v2::header_submission::SignedHeaderSubmission, BidSubmission},
    task,
    utils::utcnow_ns,
};
use helix_database::DatabaseService;
use helix_datastore::Auctioneer;
use helix_types::{BlsPublicKey, SignedBidSubmission};
use tracing::error;

use crate::Api;

/// Max time between header and payload for OptimsiticV2 submissions
const MAX_DELAY_BETWEEN_V2_SUBMISSIONS_NS: u64 = 2_000_000_000; // 2s

pub struct V2SubMessage {
    pub builder_pubkey: BlsPublicKey,
    pub slot: u64,
    pub on_receive_ns: u64,
    pub block_hash: B256,
    pub is_header: bool,
}

impl V2SubMessage {
    pub fn new_from_header_submission(
        submission: &SignedHeaderSubmission,
        on_receive_ns: u64,
    ) -> Self {
        Self {
            builder_pubkey: submission.bid_trace().builder_pubkey.clone(),
            slot: submission.slot().as_u64(),
            on_receive_ns,
            block_hash: *submission.block_hash(),
            is_header: true,
        }
    }

    pub fn new_from_block_submission(submission: &SignedBidSubmission, on_receive_ns: u64) -> Self {
        Self {
            builder_pubkey: submission.message().builder_pubkey.clone(),
            slot: submission.slot().as_u64(),
            on_receive_ns,
            block_hash: *submission.block_hash(),
            is_header: false,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PendingBlock {
    pub builder_pubkey: BlsPublicKey,
    pub slot: u64,
    pub header_receive_ns: Option<u64>,
    pub payload_receive_ns: Option<u64>,
}

pub struct V2SubChecker<A: Api> {
    v2_check_rx: tokio::sync::mpsc::UnboundedReceiver<V2SubMessage>,
    auctioneer: Arc<A::Auctioneer>,
    db: Arc<A::DatabaseService>,
    pending_blocks: HashMap<B256, PendingBlock>,
}

impl<A: Api> V2SubChecker<A> {
    pub fn new(
        v2_check_rx: tokio::sync::mpsc::UnboundedReceiver<V2SubMessage>,
        auctioneer: Arc<A::Auctioneer>,
        db: Arc<A::DatabaseService>,
    ) -> Self {
        Self { v2_check_rx, auctioneer, db, pending_blocks: HashMap::with_capacity(256) }
    }
    /// Handle valid payload Optimistic V2 demotions checks and demotions.
    ///
    /// There are two cases where we might demote a builder here.
    /// 1) They sent a header but we received no accompanying payload.
    /// 2) The payload was received > 2 seconds after we received the header.
    pub async fn run(mut self) {
        // TODO!!: local telemetry

        let mut tick = tokio::time::interval(Duration::from_secs(5));

        tokio::select! {
            _ = tick.tick() => self.check_demotions(),
            Some(msg) =self.v2_check_rx.recv() => self.process_message(msg),
        }
    }

    fn process_message(&mut self, msg: V2SubMessage) {
        let entry = self.pending_blocks.entry(msg.block_hash).or_insert(PendingBlock {
            builder_pubkey: msg.builder_pubkey,
            slot: msg.slot,
            header_receive_ns: None,
            payload_receive_ns: None,
        });

        if msg.is_header {
            match entry.header_receive_ns {
                Some(prev) => entry.header_receive_ns = Some(prev.min(msg.on_receive_ns)),
                None => entry.header_receive_ns = Some(msg.on_receive_ns),
            }
        } else {
            match entry.payload_receive_ns {
                Some(prev) => entry.payload_receive_ns = Some(prev.min(msg.on_receive_ns)),
                None => entry.payload_receive_ns = Some(msg.on_receive_ns),
            }
        }
    }

    /// Check all blocks:
    /// - for blocks with receive times, check that they are not MAX_DELAY_BETWEEN_V2_SUBMISSIONS_NS
    ///   apart,
    /// - for blocks with only header, demote if time is older than
    ///   MAX_DELAY_BETWEEN_V2_SUBMISSIONS_NS
    /// - otherwise leave
    fn check_demotions(&mut self) {
        let now_ns = utcnow_ns();
        let mut demoted = HashSet::default();

        self.pending_blocks.retain(|block_hash, pending| {
            match (pending.header_receive_ns, pending.payload_receive_ns) {
                (Some(header), Some(payload)) => {
                    let delta = header.saturating_sub(payload);
                    if delta > MAX_DELAY_BETWEEN_V2_SUBMISSIONS_NS {
                        if demoted.insert(*block_hash) {
                            demote_builder::<A>(
                                *block_hash,
                                pending.clone(),
                                self.auctioneer.clone(),
                                self.db.clone(),
                            );
                        }
                    }

                    false
                }

                (Some(t), None) | (None, Some(t)) => {
                    if now_ns.saturating_sub(t) > MAX_DELAY_BETWEEN_V2_SUBMISSIONS_NS {
                        if demoted.insert(*block_hash) {
                            demote_builder::<A>(
                                *block_hash,
                                pending.clone(),
                                self.auctioneer.clone(),
                                self.db.clone(),
                            );
                        };
                        false
                    } else {
                        // builder may still send missing message
                        true
                    }
                }

                (None, None) => unreachable!("entries have at least one timing"),
            }
        });
    }
}

fn demote_builder<A: Api>(
    block_hash: B256,
    pending_block: PendingBlock,
    auctioneer: Arc<A::Auctioneer>,
    db: Arc<A::DatabaseService>,
) {
    let reason = format!(
        "builder demoted due to missing payload submission.
{pending_block:?}"
    );

    let builder_pubkey = pending_block.builder_pubkey.clone();
    task::spawn(file!(), line!(), async move {
        if let Err(err) = auctioneer.demote_builder(&builder_pubkey).await {
            error!(%err, "failed to demote builder")
        }
    });

    task::spawn(file!(), line!(), async move {
        if let Err(err) = db
            .db_demote_builder(
                pending_block.slot,
                &pending_block.builder_pubkey,
                &block_hash,
                reason,
            )
            .await
        {
            error!(%err, "failed to demote builder")
        }
    });
}
