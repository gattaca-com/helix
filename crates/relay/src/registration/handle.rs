use std::{ops::Range, sync::Arc};

use helix_types::SignedValidatorRegistration;
use tokio::sync::oneshot;

pub struct ChannelFull;

pub struct RegWorkerJob {
    pub regs: Arc<Vec<SignedValidatorRegistration>>,
    pub range: Range<usize>,
    /// (Index in regs, has passed verification)
    pub res_tx: oneshot::Sender<Vec<(usize, bool)>>,
}

#[derive(Clone)]
pub struct RegWorkerHandle {
    worker: crossbeam_channel::Sender<RegWorkerJob>,
}

impl RegWorkerHandle {
    pub fn new(worker: crossbeam_channel::Sender<RegWorkerJob>) -> Self {
        Self { worker }
    }

    pub fn send(
        &self,
        regs: Arc<Vec<SignedValidatorRegistration>>,
        range: Range<usize>,
    ) -> Result<oneshot::Receiver<Vec<(usize, bool)>>, ChannelFull> {
        let (tx, rx) = oneshot::channel();
        self.worker.try_send(RegWorkerJob { regs, range, res_tx: tx }).map_err(|_| ChannelFull)?;
        Ok(rx)
    }
}
