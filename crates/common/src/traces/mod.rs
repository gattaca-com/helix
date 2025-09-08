pub mod builder_api_trace;
pub mod proposer_api;

use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

pub use builder_api_trace::*;
pub use proposer_api::*;

use crate::utils::utcnow_ns;

#[derive(Clone)]
pub struct RequestTimings {
    /// When the first handler started, ~ time of first packet
    pub on_receive_ns: u64,
    /// Body processing stats
    pub stats: Arc<BodyTimings>,
}

#[derive(Default)]
pub struct BodyTimings {
    // bytes read
    pub size: AtomicUsize,
    // duration in ns spent waiting to read the body
    pub wait_ns: AtomicU64,
    // duration in ns spent reading / parsing the body
    pub read_ns: AtomicU64,
    // duration in ns spent between polls
    pub gap_ns: AtomicU64,
    // timestamp in ns when the body was started reading
    pub start_ns: AtomicU64,
    // timestamp in ns when the body was finished reading
    pub finish_ns: AtomicU64,
}

impl BodyTimings {
    pub fn add_bytes(&self, n: usize) {
        self.size.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_wait(&self, d: Duration) {
        self.wait_ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn add_read(&self, d: Duration) {
        self.read_ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn add_gap(&self, d: Duration) {
        self.gap_ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn set_start(&self) {
        self.start_ns.store(utcnow_ns(), Ordering::Relaxed);
    }

    pub fn set_finish(&self) {
        self.finish_ns.store(utcnow_ns(), Ordering::Relaxed);
    }

    pub fn size(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    pub fn wait_latency(&self) -> Duration {
        Duration::from_nanos(self.wait_ns.load(Ordering::Relaxed))
    }

    pub fn gap_latency(&self) -> Duration {
        Duration::from_nanos(self.gap_ns.load(Ordering::Relaxed))
    }

    pub fn read_latency(&self) -> Duration {
        Duration::from_nanos(self.read_ns.load(Ordering::Relaxed))
    }
}
