//! Locks that say what a panic leaves behind.
//!
//! A thread that panics while holding a lock poisons it: the data may be
//! half-updated. Crashing the whole app for that is wrong, but so is
//! reading on as if nothing happened. So there is no "just lock it"
//! here: each helper names what recovery means, and the caller picks the
//! one that is true of its data.
//!
//! - [`lock_or_reset`]: the data goes back to its default. For caches and
//!   other state that is safe to forget.
//! - [`lock_or_recover`] (and [`wait_or_recover`] for a condvar): the
//!   caller's closure puts the data back into a state its users handle,
//!   such as a queue closed so its thread stops, or a sign-in that keeps
//!   its token and forgets what it derived from it.
//! - [`lock_whole`], [`read_whole`], [`write_whole`]: the data is kept as
//!   it is. Only for a value that is only ever replaced whole (a flag, an
//!   `Option` swapped in one assignment, a map changed one whole entry at a
//!   time), which a panic cannot leave torn.
//!
//! Every helper logs the recovery and clears the poison, so later locks
//! are clean and the log shows it once.

use std::sync::{
    Condvar, LockResult, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

/// Unwraps `result`, logging a poisoned lock and running `recover` on its
/// data; `clear` clears the poison afterwards.
fn recovered<G, T: ?Sized>(
    result: LockResult<G>,
    recover: impl FnOnce(&mut G),
    clear: impl FnOnce(),
) -> G {
    result.unwrap_or_else(|poisoned| {
        log::warn!(
            "a lock on {} was held by a thread that panicked; recovering",
            std::any::type_name::<T>()
        );
        let mut guard = poisoned.into_inner();
        recover(&mut guard);
        clear();
        guard
    })
}

/// Locks `mutex`; if a holder panicked, the data is reset to its default
/// first (for caches and other state that is safe to forget).
pub fn lock_or_reset<T: Default>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    lock_or_recover(mutex, |data| *data = T::default())
}

/// Locks `mutex`; if a holder panicked, `recover` first puts the data
/// back into a state its users handle.
pub fn lock_or_recover<T: ?Sized>(
    mutex: &Mutex<T>,
    recover: impl FnOnce(&mut T),
) -> MutexGuard<'_, T> {
    recovered::<_, T>(
        mutex.lock(),
        |guard| recover(guard),
        || mutex.clear_poison(),
    )
}

/// Waits on `condvar` with `guard`, a lock on `mutex`; if a holder panicked
/// meanwhile, `recover` first puts the data back into a state its users
/// handle, as [`lock_or_recover`] does.
pub fn wait_or_recover<'a, T>(
    condvar: &Condvar,
    mutex: &Mutex<T>,
    guard: MutexGuard<'a, T>,
    recover: impl FnOnce(&mut T),
) -> MutexGuard<'a, T> {
    recovered::<_, T>(
        condvar.wait(guard),
        |guard| recover(guard),
        || mutex.clear_poison(),
    )
}

/// Locks `mutex`, keeping its data as it is if a holder panicked. Only
/// for data that is only ever replaced whole, so a panic cannot leave it
/// half-updated.
pub fn lock_whole<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    recovered::<_, T>(mutex.lock(), |_| {}, || mutex.clear_poison())
}

/// Locks `lock` for reading, keeping its data as it is if a writer
/// panicked. Only for data that is only ever replaced whole, as for
/// [`lock_whole`].
pub fn read_whole<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    recovered::<_, T>(lock.read(), |_| {}, || lock.clear_poison())
}

/// Locks `lock` for writing, keeping its data as it is if a writer
/// panicked. Only for data that is only ever replaced whole, as for
/// [`lock_whole`].
pub fn write_whole<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    recovered::<_, T>(lock.write(), |_| {}, || lock.clear_poison())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A mutex holding `value` whose holder panicked.
    fn poisoned<T: Send + 'static>(value: T) -> Arc<Mutex<T>> {
        let shared = Arc::new(Mutex::new(value));
        let held = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison it");
        })
        .join();
        assert!(shared.is_poisoned());
        shared
    }

    /// An RwLock holding `value` whose writer panicked.
    fn poisoned_rw<T: Send + Sync + 'static>(value: T) -> Arc<RwLock<T>> {
        let shared = Arc::new(RwLock::new(value));
        let held = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = held.write();
            panic!("poison it");
        })
        .join();
        assert!(shared.is_poisoned());
        shared
    }

    #[test]
    fn a_poisoned_lock_comes_back_reset() {
        let shared = poisoned(vec![1, 2]);
        assert!(lock_or_reset(&shared).is_empty());
        assert!(!shared.is_poisoned());
        lock_or_reset(&shared).push(3);
        assert_eq!(*lock_or_reset(&shared), [3]);
    }

    #[test]
    fn a_poisoned_lock_comes_back_as_recovered() {
        let shared = poisoned((true, vec![1, 2]));
        let recover = |data: &mut (bool, Vec<i32>)| data.1.clear();
        assert_eq!(*lock_or_recover(&shared, recover), (true, Vec::new()));
        assert!(!shared.is_poisoned());
    }

    #[test]
    fn a_wait_on_a_poisoned_lock_comes_back_as_recovered() {
        let shared = Arc::new((Mutex::new(vec![1]), Condvar::new()));
        let held = Arc::clone(&shared);
        let mut guard = shared.0.lock().expect("a fresh lock");
        let poisoner = std::thread::spawn(move || {
            let mut guard = held.0.lock().expect("released by the wait");
            guard.push(2);
            held.1.notify_one();
            panic!("poison it");
        });
        // A wake-up before the poisoner ran finds the data untouched.
        while !guard.is_empty() {
            guard = wait_or_recover(&shared.1, &shared.0, guard, Vec::clear);
        }
        drop(guard);
        let _ = poisoner.join();
        assert!(!shared.0.is_poisoned());
    }

    #[test]
    fn a_whole_value_is_kept() {
        let shared = poisoned(Some(7));
        assert_eq!(*lock_whole(&shared), Some(7));
        assert!(!shared.is_poisoned());
    }

    #[test]
    fn a_whole_value_behind_an_rwlock_is_kept() {
        let shared = poisoned_rw(Some(7));
        assert_eq!(*read_whole(&shared), Some(7));
        assert!(!shared.is_poisoned());
        let shared = poisoned_rw(Some(7));
        *write_whole(&shared) = None;
        assert!(!shared.is_poisoned());
        assert_eq!(*read_whole(&shared), None);
    }
}
