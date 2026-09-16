//! Cooperative cancellation. Image transaction rollback can temporarily suppress it.
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
thread_local! { static SUPPRESSED: Cell<bool> = const { Cell::new(false) }; }

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Returns whether this is the first cancellation request.
pub fn request_cancel() -> bool {
    !CANCELLED.swap(true, Ordering::Relaxed)
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed) && !SUPPRESSED.get()
}

pub fn check_cancelled() -> anyhow::Result<()> {
    if is_cancelled() {
        Err(Cancelled.into())
    } else {
        Ok(())
    }
}

/// Ignore cancellation on this thread while restoring original names. The guard
/// restores the previous state even on panic; nested suppression is supported.
pub fn without_cancellation<F: FnOnce() -> T, T>(operation: F) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            SUPPRESSED.set(self.0);
        }
    }
    let _reset = Reset(SUPPRESSED.replace(true));
    operation()
}
