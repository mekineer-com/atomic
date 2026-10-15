//! Shared background runtime for all fire-and-forget work.
//!
//! Isolates background processing (embedding, tagging, ingestion) from the
//! server's request-handling runtime. Provides resource-aware semaphores so
//! concurrent API calls stay within safe limits regardless of caller.

use std::sync::LazyLock;
use tokio::sync::{watch, Semaphore};

/// Completion accounting only: callers must stop producers before waiting.
#[derive(Clone)]
pub struct WorkTracker {
    active: watch::Sender<usize>,
}

impl Default for WorkTracker {
    fn default() -> Self {
        Self {
            active: watch::channel(0).0,
        }
    }
}

impl WorkTracker {
    pub fn start(&self) -> WorkGuard {
        self.active.send_modify(|count| *count += 1);
        WorkGuard(self.active.clone())
    }

    pub async fn wait_idle(&self) {
        let mut active = self.active.subscribe();
        active
            .wait_for(|count| *count == 0)
            .await
            .expect("tracker closed");
    }
}

pub struct WorkGuard(watch::Sender<usize>);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

static WORK: LazyLock<WorkTracker> = LazyLock::new(WorkTracker::default);

/// Wait after request/scheduler producers stop; nested tasks remain counted.
pub async fn wait_idle() {
    WORK.wait_idle().await;
}

/// Shared background runtime — 4 worker threads, isolated from server/Tauri runtimes.
static BACKGROUND: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("atomic-bg")
        .enable_all()
        .build()
        .expect("Failed to create background runtime")
});

/// Concurrency limit for LLM API calls (tagging, wiki, chat tool calls).
pub static LLM_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// Concurrency limit for embedding API calls.
pub static EMBEDDING_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

/// Concurrency limit for embedding batch tasks (limits memory from queued content).
pub static EMBEDDING_BATCH_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

/// Concurrency limit for HTTP fetches (ingestion pipeline).
pub static FETCH_SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));

/// Spawn a fire-and-forget task on the shared background runtime.
/// Works from any context (sync or async).
pub fn spawn<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let work = WORK.start();
    BACKGROUND.spawn(async move {
        let _work = work;
        future.await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completion_includes_children_and_unwinding() {
        let tracker = WorkTracker::default();
        let parent = tracker.start();
        let child = tracker.start();
        drop(parent);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), tracker.wait_idle())
                .await
                .is_err()
        );
        let panicked = tokio::spawn(async move {
            let _child = child;
            panic!("fictional worker failure");
        });
        assert!(panicked.await.unwrap_err().is_panic());
        tracker.wait_idle().await;
    }

    #[tokio::test]
    async fn executor_drain_waits_for_nested_detached_work() {
        let (parent_done_tx, parent_done) = tokio::sync::oneshot::channel();
        let (release_tx, release) = tokio::sync::oneshot::channel();
        spawn(async move {
            spawn(async move {
                let _ = release.await;
            });
            let _ = parent_done_tx.send(());
        });
        parent_done.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), wait_idle())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        wait_idle().await;
    }
}
