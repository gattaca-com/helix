use std::future::Future;

use tokio::task::JoinHandle;

pub fn spawn<F>(file: &str, line: u32, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let label = format!("{file}:{line}");

    tokio::spawn(async move {
        // TODO perf: preload metrics for labels.
        let metric = crate::metrics::TASK_COUNT
            .get_metric_with_label_values(&[label.as_str()])
            .expect("Failed to get metric!");
        metric.inc();
        let result = future.await;
        metric.dec();
        result
    })
}
