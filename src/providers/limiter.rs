//! How many model requests each provider is sent at once (SME-162).
//!
//! A provider with a max concurrent requests has a pool of that many
//! slots, kept here by provider id. A model call looks its provider's
//! pool up when it starts (`Endpoint::slot`, `anthropic::stream`) and
//! holds a slot from before it sends until it ends, so a request over the
//! limit waits in smelt instead of queueing, silently, on a busy server
//! until it times out. The pools are in memory, set from the column and
//! the server's report (`resolved_max`) by whatever changes either, and by
//! every endpoint build, so a restart rebuilds them as they're used.
//!
//! A change of limit applies from each running turn's next model call,
//! not its next turn (SME-162 review 1), and resizes the provider's pool
//! in place (review 2): raised, it frees the new slots; lowered, it takes
//! back the free ones and, for the rest, the next ones its running calls
//! give back. So a change never lets more requests in than the new limit,
//! counting the ones already running. Only clearing the limit closes the
//! pool, which lets the calls waiting on it go.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use chrono::NaiveDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A provider's request slots.
pub struct Pool {
    slots: Arc<Semaphore>,
    /// Slots a lowered limit still has to take back: the next that many
    /// a call gives back are dropped instead of freed.
    owed: AtomicUsize,
}

impl Pool {
    fn new(count: u32) -> Arc<Self> {
        Arc::new(Self { slots: Arc::new(Semaphore::new(count as usize)), owed: AtomicUsize::new(0) })
    }

    /// From `from` slots to `to`.
    fn resize(&self, from: u32, to: u32) {
        let (from, to) = (from as usize, to as usize);
        if to > from {
            let mut more = to - from;
            // What's still owed is paid off first.
            if let Ok(owed) = self.owed.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |owed| Some(owed - owed.min(more))) {
                more -= owed.min(more);
            }
            self.slots.add_permits(more);
        } else {
            let fewer = from - to;
            let taken = self.slots.forget_permits(fewer);
            self.owed.fetch_add(fewer - taken, Ordering::SeqCst);
        }
    }

    /// Waits for a slot; `None` once the pool is closed (its limit was
    /// cleared).
    pub async fn acquire(self: &Arc<Self>) -> Option<Slot> {
        let permit = self.slots.clone().acquire_owned().await.ok()?;
        Some(Slot { permit: Some(permit), pool: self.clone() })
    }

    /// A slot if one is free now.
    #[cfg(test)]
    pub fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        Some(Slot { permit: Some(permit), pool: self.clone() })
    }

    #[cfg(test)]
    pub fn available_permits(&self) -> usize {
        self.slots.available_permits()
    }

    #[cfg(test)]
    pub fn is_closed(&self) -> bool {
        self.slots.is_closed()
    }
}

/// A held slot: freed when it drops, or, while a lowered limit is owed
/// slots, taken back.
pub struct Slot {
    permit: Option<OwnedSemaphorePermit>,
    pool: Arc<Pool>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let owed = self.pool.owed.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |owed| owed.checked_sub(1));
        if owed.is_ok()
            && let Some(permit) = self.permit.take()
        {
            permit.forget();
        }
    }
}

/// A provider's limit as of a version of its row, and its slots.
struct Limit {
    /// The row's `updated_at` it was set from: a set from an older row (a
    /// turn that read it before a save) is ignored (SME-162 review 1).
    version: NaiveDateTime,
    /// The count and its slots; `None` for no limit.
    pool: Option<(u32, Arc<Pool>)>,
}

static LIMITS: LazyLock<Mutex<HashMap<i64, Limit>>> = LazyLock::new(Default::default);

/// Sets how many requests at once a provider is sent, `None` for no
/// limit, as of `version` (its row's `updated_at`); a set from an older
/// version than the last is ignored. A new count resizes the provider's
/// pool in place; no limit closes it, so the calls waiting on it go.
pub fn set(provider_id: i64, version: NaiveDateTime, max: Option<u32>) {
    // Nothing panics while it's held, so a poisoned lock's map is whole.
    let mut limits = LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    let known = limits.get(&provider_id);
    if known.is_some_and(|limit| limit.version > version) {
        return;
    }
    let old = known.and_then(|limit| limit.pool.clone());
    let pool = match (old, max) {
        (Some((from, pool)), Some(to)) => {
            pool.resize(from, to);
            Some((to, pool))
        }
        (old, max) => {
            if let Some((_, pool)) = old {
                pool.slots.close();
            }
            max.map(|count| (count, Pool::new(count)))
        }
    };
    limits.insert(provider_id, Limit { version, pool });
}

/// The provider's pool now, `None` when it has no limit.
pub fn current(provider_id: i64) -> Option<Arc<Pool>> {
    let limits = LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    limits.get(&provider_id).and_then(|limit| limit.pool.as_ref()).map(|(_, pool)| pool.clone())
}

/// Forgets a deleted provider's pool. It isn't closed: requests holding
/// its slots keep them until they end, and requests waiting on it still
/// take their turn (SME-162 review 2), rather than all going at once to
/// a server that has as few slots as before.
pub fn forget(provider_id: i64) {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner()).remove(&provider_id);
}
