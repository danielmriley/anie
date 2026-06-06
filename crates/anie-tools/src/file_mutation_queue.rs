use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};

use dashmap::DashMap;
use tokio::sync::Mutex;

/// Serialize concurrent file mutations on a canonicalized path basis.
pub struct FileMutationQueue {
    locks: DashMap<PathBuf, Arc<Mutex<()>>>,
}

impl FileMutationQueue {
    /// Create an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locks: DashMap::new(),
        }
    }

    /// Canonicalize a path for lock-key purposes.
    #[must_use]
    pub fn canonicalize_path(&self, path: &Path) -> PathBuf {
        canonicalize_best_effort(path)
    }

    /// Run an async operation while holding the path-specific lock.
    pub async fn with_lock<F, Fut, T>(&self, path: &Path, operation: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let paths = [path.to_path_buf()];
        self.with_locks(&paths, operation).await
    }

    /// Run an async operation while holding the locks for **all** of
    /// `paths` at once — the primitive behind atomic multi-file
    /// mutations (`apply_patch`). Keys are canonicalized, deduped, and
    /// sorted before acquisition so two callers can never deadlock by
    /// taking the same locks in different orders, and a path repeated
    /// within one call cannot self-deadlock.
    pub async fn with_locks<F, Fut, T>(&self, paths: &[PathBuf], operation: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let mut keys: Vec<PathBuf> = paths
            .iter()
            .map(|path| self.canonicalize_path(path))
            .collect();
        keys.sort();
        keys.dedup();

        // `lock_owned` yields guards that own an `Arc` clone, so the
        // Vec of guards carries no borrow of `self.locks` and can be
        // held across the operation without lifetime gymnastics.
        let mut guards = Vec::with_capacity(keys.len());
        for key in keys {
            let lock = self
                .locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            guards.push(lock.lock_owned().await);
        }

        let result = operation().await;
        drop(guards);
        result
    }
}

impl Default for FileMutationQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    if let Some(parent) = path.parent()
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
        && let Some(file_name) = path.file_name()
    {
        return canonical_parent.join(file_name);
    }

    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(current_dir) = std::env::current_dir() {
        current_dir.join(path)
    } else {
        path.to_path_buf()
    }
}
