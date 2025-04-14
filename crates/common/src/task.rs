use std::{future::Future, sync::OnceLock};

use tokio::{runtime::Handle, task::JoinHandle};

static RUNTIME: OnceLock<Handle> = OnceLock::new();

pub fn init_runtime() {
    RUNTIME.get_or_init(build_tokio_runtime);
}

pub fn spawn<F>(file: &str, line: u32, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let label = format!("{file}:{line}");
    match RUNTIME.get() {
        Some(runtime) => runtime.spawn(async move {
            // TODO perf: preload metrics for labels.
            let metric = crate::metrics::TASK_COUNT
                .get_metric_with_label_values(&[label.as_str()])
                .expect("Failed to get metric!");
            metric.inc();
            let result = future.await;
            metric.dec();
            result
        }),
        None => panic!("runtime has not been initialised!"),
    }
}

pub fn build_tokio_runtime() -> Handle {
    if let Ok(handle) = Handle::try_current() {
        return handle;
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create runtime")
        .handle()
        .clone()
}
