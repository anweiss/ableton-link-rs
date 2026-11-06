//! Panic-free mutex helpers.
//!
//! The crate previously used `mutex.try_lock().unwrap()` throughout. That panics
//! on *any* momentary contention — and in a host built with `panic = "abort"` a
//! panic on a background Link thread takes the whole process down. It also made
//! poisoning fatal: one panic while a lock was held would turn every later
//! acquisition into a panic too.
//!
//! `lock()` blocks instead (these are all short, non-async critical sections) and
//! recovers the guard if a previous holder panicked. Poison recovery is the right
//! call here: the protected values are plain protocol state (session lists,
//! timelines, peer vectors) that stay structurally valid, and a music-sync library
//! should keep running rather than abort its host.
//!
//! Callers must still avoid holding a guard across an `.await` — the compiler
//! enforces that for `Send` futures, since `MutexGuard` is not `Send`.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock a mutex, recovering the guard if a previous holder panicked.
pub fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Read-lock an `RwLock`, recovering the guard if a previous holder panicked.
#[allow(dead_code)]
pub fn read<T: ?Sized>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Write-lock an `RwLock`, recovering the guard if a previous holder panicked.
#[allow(dead_code)]
pub fn write<T: ?Sized>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
