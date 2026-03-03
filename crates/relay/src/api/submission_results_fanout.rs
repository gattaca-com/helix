use std::{
    sync::{Arc, OnceLock},
    task::Poll,
};

use flux::tile::Tile;
use flux_utils::SharedVector;
use futures::task::AtomicWaker;

use crate::{
    HelixSpine,
    auctioneer::{SubmissionRef, SubmissionResultWithRef},
    spine::messages::SubmissionResultIx,
};

pub struct FutureBidSubmissionResult {
    result_ix: OnceLock<usize>,
    waker: AtomicWaker,
}

impl Default for FutureBidSubmissionResult {
    fn default() -> Self {
        Self::new()
    }
}

impl FutureBidSubmissionResult {
    pub fn new() -> Self {
        Self { result_ix: OnceLock::new(), waker: AtomicWaker::new() }
    }

    fn set(&self, ix: usize) {
        let _ = self.result_ix.set(ix);
        self.waker.wake();
    }

    pub fn wait(slot: Arc<Self>) -> impl Future<Output = usize> {
        std::future::poll_fn(move |cx| {
            if let Some(&ix) = slot.result_ix.get() {
                return Poll::Ready(ix);
            }
            slot.waker.register(cx.waker());
            match slot.result_ix.get() {
                Some(&ix) => Poll::Ready(ix),
                None => Poll::Pending,
            }
        })
    }
}

pub struct SubmissionResultsFanOut {
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
    submission_results: Arc<SharedVector<SubmissionResultWithRef>>,
}

impl SubmissionResultsFanOut {
    pub fn new(
        future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
        submission_results: Arc<SharedVector<SubmissionResultWithRef>>,
    ) -> Self {
        Self { future_results, submission_results }
    }
}

impl Tile<HelixSpine> for SubmissionResultsFanOut {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|SubmissionResultIx { ix }, _producers| {
            let Some(result) = self.submission_results.get(ix) else { return };
            let SubmissionRef::Http(future_ix) = result.sub_ref else { return };
            let Some(future) = self.future_results.get(future_ix) else { return };
            future.set(ix);
        });
    }
}
