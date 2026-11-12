//! Lock helpers applying one poisoning policy to the state shared with background threads.
//!
//! A panic while a lock is held must never take the UI down on the next frame. Every value
//! shared this way is either a slot written in a single assignment (assets, fonts, spell
//! status and checker) or data whose invariants hold between statements (the warm-up map,
//! the spell checker and its word cache), so the value inside a poisoned lock is still valid
//! and is used as is. The panics themselves are reported where they happen (for example
//! `spell::run_guarded` turns a loader panic into a failed status).

use std::sync::{
    Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

/// Locks `mutex`, recovering the value of a poisoned lock.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Read-locks `lock`, recovering the value of a poisoned lock.
pub fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Write-locks `lock`, recovering the value of a poisoned lock.
pub fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Waits on `ready` while `condition` holds, recovering the value of a poisoned lock.
pub fn wait_while<'a, T>(
    ready: &Condvar,
    guard: MutexGuard<'a, T>,
    condition: impl FnMut(&mut T) -> bool,
) -> MutexGuard<'a, T> {
    ready
        .wait_while(guard, condition)
        .unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn poisoned_locks_still_give_their_value() {
        let mutex = Arc::new(Mutex::new(1));
        let rw = Arc::new(RwLock::new(2));
        let (m, r) = (Arc::clone(&mutex), Arc::clone(&rw));
        let crashed = std::thread::spawn(move || {
            let _m = m.lock().unwrap();
            let _r = r.write().unwrap();
            panic!("background thread crashes while holding the locks");
        })
        .join();
        assert!(crashed.is_err());
        assert!(mutex.is_poisoned() && rw.is_poisoned());
        *lock(&mutex) += 1;
        assert_eq!(*lock(&mutex), 2);
        *write(&rw) += 1;
        assert_eq!(*read(&rw), 3);
    }
}
