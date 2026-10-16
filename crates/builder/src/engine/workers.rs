use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

type Job = Box<dyn FnOnce() + Send>;

const DEFAULT_WORKERS: usize = 8;
const SPIN: Duration = Duration::from_millis(100);

pub static CORES: OnceLock<Vec<usize>> = OnceLock::new();

pub static WORKERS: std::sync::LazyLock<Workers> = std::sync::LazyLock::new(|| {
    let cores = CORES.get().cloned().unwrap_or_default();
    let count = if cores.is_empty() { DEFAULT_WORKERS } else { cores.len() };
    Workers::spawn(count, &cores)
});

pub struct Workers {
    tx: Sender<Job>,
    count: usize,
}

struct Batch {
    next: AtomicUsize,
    done: AtomicUsize,
    len: usize,
    panic: Mutex<Option<Box<dyn Any + Send>>>,
}

impl Batch {
    fn work(&self, f: &(dyn Fn(usize) + Sync)) {
        loop {
            let ix = self.next.fetch_add(1, Ordering::Relaxed);
            if ix >= self.len {
                return;
            }
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| f(ix))) &&
                let Ok(mut slot) = self.panic.lock()
            {
                slot.get_or_insert(panic);
            }
            self.done.fetch_add(1, Ordering::Release);
        }
    }
}

impl Workers {
    fn spawn(count: usize, cores: &[usize]) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Job>(1024);
        for i in 0..count {
            let (rx, core) = (rx.clone(), cores.get(i).copied());
            std::thread::Builder::new()
                .name(format!("b-order-{i}"))
                .stack_size(super::builder::EVM_STACK)
                .spawn(move || {
                    if let Some(core) = core {
                        core_affinity::set_for_current(core_affinity::CoreId { id: core });
                    }
                    while let Some(job) = next(&rx) {
                        let _unused = catch_unwind(AssertUnwindSafe(job));
                    }
                })
                .expect("failed to spawn order worker");
        }
        Workers { tx, count }
    }

    pub fn spawn_job(&self, job: impl FnOnce() + Send + 'static) {
        if let Err(err) = self.tx.try_send(Box::new(job)) {
            (err.into_inner())();
        }
    }

    pub fn run(&self, len: usize, f: &(dyn Fn(usize) + Sync)) {
        let batch = Arc::new(Batch {
            next: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            len,
            panic: Mutex::new(None),
        });
        let shared: &'static (dyn Fn(usize) + Sync) = unsafe { std::mem::transmute(f) };
        for _ in 0..len.saturating_sub(1).min(self.count) {
            let batch = batch.clone();
            if self.tx.try_send(Box::new(move || batch.work(shared))).is_err() {
                break;
            }
        }
        batch.work(f);
        while batch.done.load(Ordering::Acquire) < len {
            std::hint::spin_loop();
        }
        if let Some(panic) = batch.panic.lock().ok().and_then(|mut slot| slot.take()) {
            resume_unwind(panic);
        }
    }
}

fn next(rx: &Receiver<Job>) -> Option<Job> {
    let idle = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(job) => return Some(job),
            Err(TryRecvError::Disconnected) => return None,
            Err(TryRecvError::Empty) if idle.elapsed() > SPIN => return rx.recv().ok(),
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
        }
    }
}
