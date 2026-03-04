mod api;
mod auctioneer;
mod beacon;
mod bid_decoder;
mod gossip;
mod housekeeper;
mod network;
mod spine;
mod tcp_bid_recv;
mod website;

use std::time::Duration;

use helix_common::metrics::{
    TOKIO_ALIVE_TASKS, TOKIO_GLOBAL_QUEUE_DEPTH, TOKIO_SCHEDULING_DRIFT, TOKIO_WORKER_BUSY_DURATION,
};
pub use helix_database::{
    DbRequest, PendingBlockSubmissionValue, PostgresDatabaseService, handle::DbHandle,
    start_db_service,
};
use tokio::time::Instant;

pub use crate::{
    api::{
        Api, BidAdjustor, DefaultBidAdjustor, start_admin_service, start_api_service,
        submission_results_fanout::{FutureBidSubmissionResult, SubmissionResultsFanOut},
    },
    auctioneer::{
        Auctioneer, AuctioneerHandle, BidSorter, BlockSimRequest, Context, Event,
        InternalBidSubmission, PayloadEntry, RegWorker, RegWorkerHandle, SimulatorClient,
        SimulatorManager, SimulatorRequest, SlotData, SubWorker, SubmissionPayload,
        SubmissionResultWithRef,
    },
    beacon::start_beacon_client,
    bid_decoder::{DecoderTile, SubmissionDataWithSpan},
    housekeeper::start_housekeeper,
    network::RelayNetworkManager,
    spine::HelixSpine,
    tcp_bid_recv::{
        BidSubmissionFlags, BidSubmissionHeader, BidSubmissionResponse, BidSubmissionTcpListener,
        RegistrationMsg, S3PayloadSaver,
    },
    website::WebsiteService,
};

pub fn spawn_tokio_monitoring() {
    tokio::spawn(async move {
        let metrics = tokio::runtime::Handle::current().metrics();
        let mut prev_busy_us = vec![0; metrics.num_workers()];
        let sleeping_duration = Duration::from_secs(1);
        loop {
            let prev = Instant::now();
            tokio::time::sleep(sleeping_duration).await;
            let slept_time = prev.elapsed();

            let scheduling_drift = slept_time.saturating_sub(sleeping_duration).as_millis();
            TOKIO_SCHEDULING_DRIFT.set(scheduling_drift as f64);

            let metrics = tokio::runtime::Handle::current().metrics();

            TOKIO_ALIVE_TASKS.set(metrics.num_alive_tasks() as f64);
            TOKIO_GLOBAL_QUEUE_DEPTH.set(metrics.global_queue_depth() as f64);

            for w in 0..metrics.num_workers() {
                let busy = metrics.worker_total_busy_duration(w).as_micros();
                TOKIO_WORKER_BUSY_DURATION
                    .with_label_values(&[&w.to_string()])
                    .set((busy - prev_busy_us[w]) as f64);

                prev_busy_us[w] = busy;
            }
        }
    });
}
