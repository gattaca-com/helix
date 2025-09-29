use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    thread::{self, ThreadId},
};

use parking_lot::Mutex;
use tokio::{runtime, task::JoinHandle};
use tracing::{info, Instrument};

use crate::utils::pin_thread_to_core;

pub fn spawn<F>(file: &str, line: u32, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let label = format!("{file}:{line}");

    tokio::spawn(
        async move {
            // TODO perf: preload metrics for labels.
            let metric = crate::metrics::TASK_COUNT
                .get_metric_with_label_values(&[label.as_str()])
                .expect("Failed to get metric!");
            metric.inc();
            let result = future.await;
            metric.dec();
            result
        }
        .in_current_span(),
    )
}

// Auctioneer core, worker cores
pub type RelayCores = (usize, Vec<usize>);

pub fn init_runtime(
    is_submission_instance: bool,
    worker_threads: usize,
) -> (runtime::Runtime, Option<RelayCores>) {
    if is_submission_instance {
        let num_cpus = num_cpus::get_physical();
        let num_tokio_cores = num_cpus.saturating_sub(worker_threads + 1);

        assert!(num_tokio_cores > 0, "no core left for tokio runtime");

        let tokio_cores = (0..num_tokio_cores).collect();
        let auctioneer_core = num_tokio_cores;
        let worker_cores = (num_tokio_cores + 1..num_cpus).collect();

        info!("initializing tokio runtime with {num_tokio_cores} cores");

        let cores_a = Arc::new(Mutex::new(Cores::new(tokio_cores)));
        let cores_b = cores_a.clone();

        let runtime = runtime::Builder::new_multi_thread()
            .thread_name_fn(move || {
                static COUNT: AtomicU32 = AtomicU32::new(0);
                format!("tokio-{}", COUNT.fetch_add(1, Ordering::Relaxed))
            })
            .enable_all()
            .worker_threads(num_tokio_cores)
            .max_blocking_threads(1) // don't use spawn_blocking on a submission instance
            .on_thread_start(move || {
                let thread_id = thread::current().id();
                let (core, _count) = cores_a.lock().add(thread_id);
                pin_thread_to_core(core);
            })
            .on_thread_stop(move || {
                let thread_id = thread::current().id();
                cores_b.lock().remove(thread_id);
            })
            .build()
            .unwrap();

        (runtime, Some((auctioneer_core, worker_cores)))
    } else {
        info!("initializing default tokio runtime");
        let runtime = runtime::Builder::new_multi_thread()
            .thread_name_fn(move || {
                static COUNT: AtomicU32 = AtomicU32::new(0);
                format!("tokio-{}", COUNT.fetch_add(1, Ordering::Relaxed))
            })
            .enable_all()
            .build()
            .unwrap();

        (runtime, None)
    }
}

/// Helper struct for managing free cores for Tokio.
#[derive(Default)]
struct Cores {
    by_id: HashMap<ThreadId, usize>,
    counts: HashMap<usize, usize>,
}

impl Cores {
    fn new(cores: Vec<usize>) -> Self {
        let by_id = HashMap::new();
        let mut counts = HashMap::new();
        cores.into_iter().for_each(|core| {
            counts.insert(core, 0);
        });

        Self { by_id, counts }
    }

    fn remove(&mut self, thread: ThreadId) {
        if let Some(core) = self.by_id.remove(&thread) {
            self.counts.get_mut(&core).map(|count| *count -= 1);
        }
    }

    fn add(&mut self, thread: ThreadId) -> (usize, usize) {
        let (core, _) =
            self.counts.iter().min_by(|(_, a), (_, b)| a.cmp(b)).expect("cores map is empty!");

        let core = *core;

        self.by_id.insert(thread, core);

        let count = self.counts.entry(core).and_modify(|count| *count += 1).or_insert(1);

        (core, *count)
    }
}
