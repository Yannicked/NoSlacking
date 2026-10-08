//! Locks that keep going after a panic.
//!
//! A thread that panics while holding a lock poisons it. Everything this
//! app guards is either replaced whole or harmless to read half-updated
//! (caches, pictures, flags), so carrying on with what the panicked holder
//! left beats taking the whole app down with it.

use std::sync::{
    Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

/// Locks `mutex`, going on with its data if a holder panicked.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Locks `lock` for reading, going on with its data if a writer panicked.
pub fn read<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Locks `lock` for writing, going on with its data if a writer panicked.
pub fn write<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Waits on `condvar`, going on with the data if a holder panicked.
pub fn wait<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar.wait(guard).unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_lock_still_opens() {
        let shared = Arc::new(Mutex::new(1));
        let held = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison it");
        })
        .join();
        assert!(shared.is_poisoned());
        *lock(&shared) += 1;
        assert_eq!(*lock(&shared), 2);
    }

    #[test]
    fn a_poisoned_rwlock_still_opens() {
        let shared = Arc::new(RwLock::new(1));
        let held = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = held.write();
            panic!("poison it");
        })
        .join();
        assert!(shared.is_poisoned());
        *write(&shared) += 1;
        assert_eq!(*read(&shared), 2);
    }
}
