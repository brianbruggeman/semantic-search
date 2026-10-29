use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use proxima::pipe::Pipe;
use thiserror::Error;
use tokio::sync::Semaphore;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdmitError<Inner> {
    #[error("busy: all slots taken and the wait queue is full")]
    Busy,
    #[error(transparent)]
    Inner(Inner),
}

/// Shared view of a gate for status reporting.
#[derive(Debug, Default)]
pub struct AdmitGauge {
    running: AtomicUsize,
    waiting: AtomicUsize,
    rejected: AtomicU64,
}

impl AdmitGauge {
    #[must_use]
    pub fn running(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
}

/// Bounded admission around any pipe: at most `cap` calls run, up to `depth` more wait FIFO
/// (the semaphore is fair), and anything beyond is answered at once with [`AdmitError::Busy`].
///
/// This wraps rather than chains because the slot must be held across the inner call, which
/// `and_then` cannot express. `CreditGate` is the nearest existing pipe: it wakes every waiter
/// (`notify_waiters`) and has no depth bound, so it can neither order callers nor shed them.
pub struct Admit<Inner> {
    inner: Inner,
    slots: Arc<Semaphore>,
    depth: usize,
    gauge: Arc<AdmitGauge>,
}

impl<Inner> Admit<Inner> {
    #[must_use]
    pub fn new(inner: Inner, cap: usize, depth: usize) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(cap.max(1))),
            depth,
            gauge: Arc::default(),
        }
    }

    #[must_use]
    pub fn gauge(&self) -> Arc<AdmitGauge> {
        Arc::clone(&self.gauge)
    }
}

impl<Inner: Pipe> Pipe for Admit<Inner> {
    type In = Inner::In;
    type Out = Inner::Out;
    type Err = AdmitError<Inner::Err>;

    async fn call(&self, input: Self::In) -> Result<Self::Out, Self::Err> {
        let permit = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => self.wait_for_slot().await.ok_or(AdmitError::Busy)?,
        };
        self.gauge.running.fetch_add(1, Ordering::Relaxed);
        let outcome = self.inner.call(input).await;
        self.gauge.running.fetch_sub(1, Ordering::Relaxed);
        drop(permit);
        outcome.map_err(AdmitError::Inner)
    }
}

impl<Inner> Admit<Inner> {
    async fn wait_for_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        if self.gauge.waiting.fetch_add(1, Ordering::AcqRel) >= self.depth {
            self.gauge.waiting.fetch_sub(1, Ordering::AcqRel);
            self.gauge.rejected.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let permit = Arc::clone(&self.slots).acquire_owned().await.ok();
        self.gauge.waiting.fetch_sub(1, Ordering::AcqRel);
        permit
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use super::*;

    struct Held {
        release: Arc<tokio::sync::Notify>,
    }

    impl Pipe for Held {
        type In = u32;
        type Out = u32;
        type Err = Infallible;

        async fn call(&self, input: u32) -> Result<u32, Infallible> {
            self.release.notified().await;
            Ok(input)
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[proxima::test(runtime = "tokio")]
    async fn flood_beyond_cap_plus_depth_is_busy_and_admitted_calls_finish_in_fifo_order() {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(Admit::new(
            Held {
                release: Arc::clone(&release),
            },
            2,
            3,
        ));
        let gauge = gate.gauge();
        let order = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for id in 0..20_u32 {
            let (gate, order) = (Arc::clone(&gate), Arc::clone(&order));
            tasks.push(tokio::spawn(async move {
                let outcome = gate.call(id).await;
                order.lock().await.push(id);
                outcome
            }));
            settle().await;
        }
        let (running, waiting, rejected) = (gauge.running(), gauge.waiting(), gauge.rejected());
        for _ in 0..5 {
            release.notify_one();
            settle().await;
        }
        let mut admitted = 0;
        let mut busy = 0;
        for task in tasks {
            match task.await.expect("task") {
                Ok(_) => admitted += 1,
                Err(AdmitError::Busy) => busy += 1,
                Err(AdmitError::Inner(never)) => match never {},
            }
        }

        assert_eq!((running, waiting, rejected), (2, 3, 15));
        assert_eq!((admitted, busy), (5, 15));
        let finished = order.lock().await.clone();
        let admitted_ids: Vec<u32> = finished.iter().copied().filter(|id| *id < 5).collect();
        assert_eq!(
            admitted_ids,
            vec![0, 1, 2, 3, 4],
            "admitted calls complete in arrival order"
        );
    }
}
