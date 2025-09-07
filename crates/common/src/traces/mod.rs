pub mod builder_api_trace;
pub mod proposer_api;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

pub use builder_api_trace::*;
pub use proposer_api::*;

use crate::utils::utcnow_ns;

#[derive(Clone)]
pub struct MiddlewareTimings {
    pub on_receive_ns: u64,
    pub stats: Arc<BodyTimingStats>,
}

#[derive(Default)]
pub struct BodyTimingStats {
    // bytes read
    pub size: AtomicU64,
    // time in ns spent waiting to read the body
    pub wait_ns: AtomicU64,
    // time in ns spent reading the body
    pub read_ns: AtomicU64,
    // total time spent processing the body
    pub total_read_ns: AtomicU64,
    // timestamp in ns when the body was started reading
    pub start_ns: AtomicU64,
    // timestamp in ns when the body was finished reading
    pub finish_ns: AtomicU64,
}

impl BodyTimingStats {
    pub fn add_bytes(&self, n: usize) {
        self.size.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn add_wait(&self, d: Duration) {
        self.wait_ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn add_read(&self, d: Duration) {
        self.read_ns.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn set_start(&self) {
        self.start_ns.store(utcnow_ns(), Ordering::Relaxed);
    }

    pub fn set_finish(&self, d: Duration) {
        self.finish_ns.store(utcnow_ns(), Ordering::Relaxed);
        self.total_read_ns.store(d.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }

    pub fn wait_latency(&self) -> Duration {
        Duration::from_nanos(self.wait_ns.load(Ordering::Relaxed))
    }

    pub fn read_latency(&self) -> Duration {
        Duration::from_nanos(self.read_ns.load(Ordering::Relaxed))
    }

    pub fn total_latency(&self) -> Duration {
        Duration::from_nanos(self.total_read_ns.load(Ordering::Relaxed))
    }
}
