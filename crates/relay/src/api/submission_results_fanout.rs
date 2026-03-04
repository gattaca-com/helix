use std::{
    sync::{Arc, OnceLock},
    task::Poll,
};

use flux::tile::Tile;
use flux_utils::SharedVector;
use futures::task::AtomicWaker;

use crate::{HelixSpine, auctioneer::SubmissionRef, spine::messages::SubmissionResultWithRef};

pub struct FutureBidSubmissionResult {
    result: OnceLock<SubmissionResultWithRef>,
    waker: AtomicWaker,
}

impl Default for FutureBidSubmissionResult {
    fn default() -> Self {
        Self::new()
    }
}

impl FutureBidSubmissionResult {
    pub fn new() -> Self {
        Self { result: OnceLock::new(), waker: AtomicWaker::new() }
    }

    fn set(&self, result: SubmissionResultWithRef) {
        let _ = self.result.set(result);
        self.waker.wake();
    }

    pub fn wait(slot: Arc<Self>) -> impl Future<Output = SubmissionResultWithRef> {
        std::future::poll_fn(move |cx| {
            if let Some(&result) = slot.result.get() {
                return Poll::Ready(result);
            }
            slot.waker.register(cx.waker());
            match slot.result.get() {
                Some(&result) => Poll::Ready(result),
                None => Poll::Pending,
            }
        })
    }
}

pub struct SubmissionResultsFanOut {
    future_results: Arc<SharedVector<FutureBidSubmissionResult>>,
}

impl SubmissionResultsFanOut {
    pub fn new(future_results: Arc<SharedVector<FutureBidSubmissionResult>>) -> Self {
        Self { future_results }
    }
}

impl Tile<HelixSpine> for SubmissionResultsFanOut {
    fn loop_body(&mut self, adapter: &mut flux::spine::SpineAdapter<HelixSpine>) {
        adapter.consume(|result: SubmissionResultWithRef, _producers| {
            let SubmissionRef::Http(future_ix) = result.sub_ref else { return };
            let Some(future) = self.future_results.get(future_ix) else { return };
            future.set(result);
        });
    }
}
