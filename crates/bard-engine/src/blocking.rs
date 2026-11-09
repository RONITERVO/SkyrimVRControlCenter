//! Keep filesystem work and contended synchronous locks off async I/O workers.
use anyhow::{Context, Result};
use tokio::sync::Semaphore;

// Bound active disk/preparation jobs rather than creating a thread per request.
static SLOTS: Semaphore = Semaphore::const_new(4);

pub async fn run<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let permit = SLOTS.acquire().await.context("Blocking work unavailable")?;
    tokio::task::spawn_blocking(move || {
        // A cancelled HTTP request must not interrupt an in-progress durable journal.
        let _permit = permit;
        work()
    })
    .await
    .context("Blocking worker stopped")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn disk_wait_does_not_block_async_progress() {
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = release.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let disk = tokio::spawn(run(move || {
            let _ = started.send(());
            let (lock, changed) = &*worker_release;
            let guard = lock.lock().unwrap();
            let _guard = changed
                .wait_timeout_while(guard, Duration::from_secs(3), |done| !*done)
                .unwrap();
            Ok(())
        }));
        ready.await.unwrap();
        // This future runs on the sole async worker while the disk job is waiting.
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::task::yield_now().await;
        })
        .await
        .unwrap();
        assert!(!disk.is_finished());
        *release.0.lock().unwrap() = true;
        release.1.notify_one();
        disk.await.unwrap().unwrap();
    }
}
