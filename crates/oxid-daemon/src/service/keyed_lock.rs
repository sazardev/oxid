//! Per-key mutexes, so work that cannot conflict does not wait on work that
//! cannot conflict with it either.
//!
//! Every lifecycle operation used to serialize behind one process-wide
//! mutex. That was correct — the races it closed are real — but far
//! stronger than the races require: two branches of two different projects
//! share no git checkout, no container name and no environment row, and had
//! no reason to queue behind one another. On a node a team actually pushes
//! to, that queue *is* the throughput ceiling: fifteen branches deploying
//! took as long as fifteen builds run back to back, because that is exactly
//! what happened.
//!
//! Locks are created on first use and dropped once nobody holds one, so a
//! long-lived daemon does not accumulate an entry per branch it has ever
//! seen.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// A registry of mutexes addressed by key.
#[derive(Debug)]
pub struct KeyedLocks<K> {
    locks: StdMutex<HashMap<K, Arc<Mutex<()>>>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            locks: StdMutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> KeyedLocks<K> {
    /// Waits for exclusive access to `key`.
    ///
    /// The returned guard owns its `Arc`, so the entry stays alive for as
    /// long as it is held even though the registry may have forgotten it —
    /// which is what makes [`Self::prune`] safe to run at any time.
    ///
    /// # Panics
    /// If the registry's own mutex was poisoned, which needs a panic while
    /// a previous caller held it — there is no correct way to keep handing
    /// out locks whose bookkeeping may be torn.
    pub async fn acquire(&self, key: K) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().expect("keyed lock registry poisoned");
            Arc::clone(locks.entry(key).or_default())
        };
        let guard = lock.lock_owned().await;
        self.prune();
        guard
    }

    /// Forgets entries nobody is holding or waiting on. Called after each
    /// acquisition rather than on a timer: the map only grows when work is
    /// happening, so that is exactly when it is worth tidying.
    fn prune(&self) {
        let mut locks = self.locks.lock().expect("keyed lock registry poisoned");
        // Two references means "the map's own, plus the one the caller just
        // took" — anything higher has a holder or a waiter behind it.
        locks.retain(|_, lock| Arc::strong_count(lock) > 2 || lock.try_lock().is_err());
    }

    /// How many keys the registry currently tracks. Test-facing.
    ///
    /// # Panics
    /// If the registry's own mutex was poisoned.
    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.locks.lock().expect("poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Different keys must not wait on each other — the whole point.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_keys_run_concurrently() {
        let locks: Arc<KeyedLocks<u32>> = Arc::new(KeyedLocks::default());
        let live = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for key in 0..8 {
            let (locks, live, peak) = (locks.clone(), live.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                let _guard = locks.acquire(key).await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(60)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "eight distinct keys must overlap, not queue"
        );
    }

    /// The same key must still be exclusive — the races the single global
    /// mutex closed are real, and this is what preserves them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_same_key_stays_exclusive() {
        let locks: Arc<KeyedLocks<&'static str>> = Arc::new(KeyedLocks::default());
        let live = Arc::new(AtomicU32::new(0));
        let clashes = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (locks, live, clashes) = (locks.clone(), live.clone(), clashes.clone());
            tasks.push(tokio::spawn(async move {
                let _guard = locks.acquire("one").await;
                if live.fetch_add(1, Ordering::SeqCst) != 0 {
                    clashes.fetch_add(1, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(clashes.load(Ordering::SeqCst), 0, "one key, one holder");
    }

    /// The registry must not grow with every key ever seen — a daemon that
    /// has deployed ten thousand branches should not hold ten thousand
    /// mutexes.
    #[tokio::test]
    async fn released_keys_are_forgotten() {
        let locks: KeyedLocks<u32> = KeyedLocks::default();
        for key in 0..50 {
            let _guard = locks.acquire(key).await;
        }
        assert!(
            locks.tracked() <= 1,
            "registry kept {} entries after every guard was dropped",
            locks.tracked()
        );
    }
}
