use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, OnceLock,
    },
    thread::{self, ThreadId},
};

use parking_lot::Mutex;
use tokio::{
    runtime::{self},
    task::JoinHandle,
};
use tracing::{info, warn, Instrument};

use crate::{utils::pin_thread_to_core, CoresConfig};

static RUNTIME: OnceLock<runtime::Runtime> = OnceLock::new();

#[macro_export]
macro_rules! spawn_tracked {
    ($future:expr) => {
        helix_common::task::spawn(file!(), line!(), $future)
    };
}

pub fn spawn<F>(file: &str, line: u32, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let label = format!("{file}:{line}");
    match RUNTIME.get() {
        Some(runtime) => runtime.spawn(
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
        ),

        None => panic!("runtime has not been initialised!"),
    }
}

pub fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    match RUNTIME.get() {
        Some(runtime) => runtime.block_on(future),
        None => panic!("runtime has not been initialised!"),
    }
}

pub fn init_runtime(config: &CoresConfig) {
    assert!(config.workers.len() > 0, "need at least 1 worker core");
    assert!(config.tokio.len() > 0, "need at least 1 tokio core");
    assert!(config.tokio_blocking > 0, "need at least 1 blocking tokio thread");

    if config == &CoresConfig::default() {
        warn!("initializing default cores config, this is not recommended for production");
    }

    info!(cores = ?config.tokio, blocking_threads = config.tokio_blocking, "initializing tokio runtime");

    let cores_a = Arc::new(Mutex::new(Cores::new(config.tokio.clone())));
    let cores_b = cores_a.clone();

    let runtime = runtime::Builder::new_multi_thread()
        .thread_name_fn(move || {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            format!("tokio-{}", COUNT.fetch_add(1, Ordering::Relaxed))
        })
        .enable_all()
        .worker_threads(config.tokio.len())
        .max_blocking_threads(config.tokio_blocking)
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

    RUNTIME.set(runtime).expect("init runtime");
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
