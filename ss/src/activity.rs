use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use proxima::telemetry::error;

const COUNT_MASK: u64 = (1 << 16) - 1;
const EPOCH_STEP: u64 = 1 << 16;
const EPOCH_MAX: u64 = (1 << 47) - 1;
const POISON_BIT: u64 = 1 << 63;

/// One atomic snapshot of active owners and their generation.
#[derive(Default)]
pub(crate) struct Activity {
    state: AtomicU64,
}

pub(crate) struct ActivityLease {
    owner: Arc<Activity>,
    counted: bool,
}

impl Activity {
    pub(crate) fn begin(self: &Arc<Self>) -> ActivityLease {
        loop {
            let current = self.state.load(Ordering::Acquire);
            if current & POISON_BIT != 0 {
                return ActivityLease { owner: Arc::clone(self), counted: false };
            }
            let count = current & COUNT_MASK;
            let epoch = (current >> 16) & EPOCH_MAX;
            if count == COUNT_MASK || epoch == EPOCH_MAX {
                self.poison();
                return ActivityLease { owner: Arc::clone(self), counted: false };
            }
            if self.state.compare_exchange_weak(
                current,
                current + EPOCH_STEP + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ).is_ok() {
                return ActivityLease { owner: Arc::clone(self), counted: true };
            }
        }
    }

    pub(crate) fn quiet_epoch(&self) -> Option<u64> {
        let state = self.state.load(Ordering::Acquire);
        (state & (COUNT_MASK | POISON_BIT) == 0).then_some((state >> 16) & EPOCH_MAX)
    }

    fn poison(&self) {
        if self.state.fetch_or(POISON_BIT, Ordering::AcqRel) & POISON_BIT == 0 {
            error!("idle maintenance disabled because activity counter or epoch overflowed");
        }
    }

    fn end(&self) {
        loop {
            let current = self.state.load(Ordering::Acquire);
            if current & POISON_BIT != 0 {
                return;
            }
            let epoch = (current >> 16) & EPOCH_MAX;
            if epoch == EPOCH_MAX {
                self.poison();
                return;
            }
            if self.state.compare_exchange_weak(
                current,
                current + EPOCH_STEP - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ).is_ok() {
                return;
            }
        }
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        if self.counted {
            self.owner.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_activity_has_one_quiet_epoch_across_overlapping_owners() {
        let activity = Arc::new(Activity::default());
        let initial = activity.quiet_epoch().expect("initial quiet snapshot");
        let request = activity.begin();
        let native = activity.begin();
        drop(request);
        assert_eq!(activity.quiet_epoch(), None);
        drop(native);
        assert!(activity.quiet_epoch().is_some_and(|epoch| epoch > initial));
    }

    #[test]
    fn packed_activity_overflow_poison_disables_maintenance() {
        let activity = Arc::new(Activity::default());
        activity.state.store(COUNT_MASK, Ordering::Release);
        let lease = activity.begin();
        assert_eq!(activity.quiet_epoch(), None);
        drop(lease);
        let other = Arc::new(Activity::default());
        other.state.store(EPOCH_MAX << 16, Ordering::Release);
        drop(other.begin());
        assert_eq!(other.quiet_epoch(), None);
    }
}
