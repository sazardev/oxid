//! Shares one refresh between everybody who asked for it at the same time.
//!
//! Some work is idempotent, expensive, and answers every caller at once. A
//! `git fetch` is the example this exists for: it brings down *every* branch
//! of a repository, so when fifteen developers push fifteen branches at the
//! same moment, the first fetch has already retrieved what the other
//! fourteen are about to ask for — and they used to queue up and repeat it
//! anyway, one network round-trip each. Measured against a repository on
//! GitHub from a home connection, that was 425 ms times fourteen: roughly
//! three quarters of the wall-clock of the whole burst, spent fetching
//! commits the daemon already had.
//!
//! Sharing is decided on *when the caller asked*, not on the age of the
//! result, which is what keeps it honest. A caller records the instant it
//! wanted fresh data; it can only be served by a refresh that **began after**
//! that instant, because such a refresh cannot have missed anything the
//! caller could be waiting for. A caller arriving after the shared refresh
//! started still runs its own. Nobody is ever handed data older than their
//! own request — this is coalescing, not caching, and there is no staleness
//! window to tune.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::Mutex;

/// One key's slot: the most recent successful refresh, if any, behind the
/// lock that makes callers for that key take turns.
type Slot<V> = Arc<Mutex<Option<Completed<V>>>>;

/// A registry of coalesced refreshes, addressed by key.
#[derive(Debug)]
pub struct RefreshCoalescer<K, V> {
    entries: StdMutex<HashMap<K, Slot<V>>>,
}

/// The most recent successful refresh for one key.
#[derive(Debug, Clone)]
struct Completed<V> {
    /// When that refresh *started*. A caller who asked before this can be
    /// served by it; a caller who asked after cannot.
    started_at: Instant,
    value: V,
}

impl<K, V> Default for RefreshCoalescer<K, V> {
    fn default() -> Self {
        Self {
            entries: StdMutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> RefreshCoalescer<K, V> {
    /// Returns the result of a refresh no older than `asked_at`, running
    /// `refresh` only if no such refresh has already been done.
    ///
    /// Callers for the same key serialize, which is the point: the first one
    /// through does the work and the rest find it already done rather than
    /// repeating it. A failed refresh is not recorded, so the next caller
    /// retries rather than inheriting the failure.
    ///
    /// # Errors
    /// Whatever `refresh` returns.
    ///
    /// # Panics
    /// If the registry's own mutex was poisoned, which needs a panic while a
    /// previous caller held it.
    pub async fn run<F, Fut, E>(&self, key: K, asked_at: Instant, refresh: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let entry = {
            let mut entries = self.entries.lock().expect("refresh coalescer poisoned");
            Arc::clone(entries.entry(key).or_default())
        };
        let mut slot = entry.lock().await;

        if let Some(done) = slot.as_ref()
            && done.started_at >= asked_at
        {
            return Ok(done.value.clone());
        }

        let started_at = Instant::now();
        let value = refresh().await?;
        *slot = Some(Completed {
            started_at,
            value: value.clone(),
        });
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn coalescer() -> Arc<RefreshCoalescer<&'static str, u32>> {
        Arc::new(RefreshCoalescer::default())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_that_asked_together_share_one_refresh() {
        let c = coalescer();
        let runs = Arc::new(AtomicUsize::new(0));
        let asked_at = Instant::now();

        let calls = (0..8).map(|_| {
            let c = Arc::clone(&c);
            let runs = Arc::clone(&runs);
            async move {
                c.run("repo", asked_at, || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok::<_, ()>(7)
                })
                .await
            }
        });
        let results = futures_util::future::join_all(calls).await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the refresh ran more than once"
        );
        assert!(results.iter().all(|r| r == &Ok(7)));
    }

    #[tokio::test]
    async fn a_caller_that_asked_later_gets_its_own_refresh() {
        // The whole point: a push that happened after the shared fetch began
        // must not be answered by it.
        let c = coalescer();
        let runs = Arc::new(AtomicUsize::new(0));
        let run_once = |asked_at| {
            let c = Arc::clone(&c);
            let runs = Arc::clone(&runs);
            async move {
                c.run("repo", asked_at, || async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(7)
                })
                .await
            }
        };

        run_once(Instant::now()).await.unwrap();
        run_once(Instant::now()).await.unwrap();

        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn different_keys_do_not_share() {
        let c = coalescer();
        let runs = Arc::new(AtomicUsize::new(0));
        let asked_at = Instant::now();
        for key in ["a", "b"] {
            let runs = Arc::clone(&runs);
            c.run(key, asked_at, || async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(7)
            })
            .await
            .unwrap();
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_is_not_remembered() {
        let c = coalescer();
        let asked_at = Instant::now();
        assert_eq!(
            c.run("repo", asked_at, || async { Err::<u32, _>(()) })
                .await,
            Err(())
        );
        // Same `asked_at`: without the failure being discarded, this would
        // be served the value the failed attempt never produced.
        assert_eq!(
            c.run("repo", asked_at, || async { Ok::<_, ()>(3) }).await,
            Ok(3)
        );
    }
}
