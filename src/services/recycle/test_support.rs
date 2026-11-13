//! Path-scoped copy barrier for handler cancellation regressions.
//! Small fixtures exercise the real cross-device fallback without a
//! second filesystem, large files, sleeps, or process-wide env flags.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use tokio::sync::Notify;

static COPIES: LazyLock<Mutex<HashMap<PathBuf, Arc<CopyBarrier>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) struct CopyBarrier {
    entered: Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

impl CopyBarrier {
    pub(super) fn pause(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.wake.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

pub(super) fn copy_for(source: &Path) -> Option<Arc<CopyBarrier>> {
    COPIES.lock().unwrap().get(source).cloned()
}

pub(crate) struct PausedCopy {
    source: PathBuf,
    barrier: Arc<CopyBarrier>,
}

impl PausedCopy {
    pub(crate) fn at(source: &Path) -> Self {
        let source = source.to_path_buf();
        let barrier = Arc::new(CopyBarrier {
            entered: Notify::new(),
            released: Mutex::new(false),
            wake: Condvar::new(),
        });
        assert!(
            COPIES
                .lock()
                .unwrap()
                .insert(source.clone(), barrier.clone())
                .is_none(),
            "copy barrier already registered for {}",
            source.display()
        );
        Self { source, barrier }
    }

    pub(crate) async fn wait(&self) {
        self.barrier.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.barrier.release();
    }
}

impl Drop for PausedCopy {
    fn drop(&mut self) {
        COPIES.lock().unwrap().remove(&self.source);
        // Unblock even when an assertion fails, so Tokio can shut down.
        self.release();
    }
}
