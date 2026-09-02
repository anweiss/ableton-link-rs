// Platform-specific thread factory implementations
// Based on Ableton Link's thread optimizations
// Now using 100% safe Rust implementations!

use std::thread::{self, JoinHandle};

pub struct ThreadFactory;

impl ThreadFactory {
    /// Create a new thread with platform-specific optimizations
    /// Sets thread name for debugging purposes
    /// This is a completely safe implementation using only std::thread::Builder
    pub fn make_thread<F, T>(name: String, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Use only safe Rust thread creation
        // std::thread::Builder already sets the thread name safely
        // We don't need platform-specific naming APIs since Rust's thread names
        // are visible in debuggers and profilers
        thread::Builder::new()
            .name(name)
            .spawn(f)
            .expect("Failed to spawn thread")
    }
}

/// Raises (and can later restore) the scheduling priority of the calling
/// thread. Rust analogue of upstream's `platform::ThreadPriority`
/// (`include/ableton/platforms/{linux,darwin,windows}/ThreadFactory.hpp`),
/// intended to be called from the Link IO thread to improve clock-sync
/// quality under system load.
///
/// `set_high` captures the thread's current scheduling parameters on the first
/// call and is a no-op on subsequent calls until `reset` is called; `reset`
/// restores the captured parameters and is a no-op if nothing was captured.
/// Both are best-effort: as upstream, a failure to change scheduling is
/// ignored rather than reported.
///
/// # Implementation
///
/// This is built on the `audio_thread_priority` crate rather than on
/// hand-written FFI, so the port carries no `unsafe`. That crate drives the
/// same three OS mechanisms upstream uses:
///
/// | Platform | Mechanism | Upstream |
/// |----------|-----------|----------|
/// | Linux | `pthread_setschedparam` with `SCHED_FIFO` | same |
/// | macOS, iOS | mach `thread_policy_set`, `THREAD_TIME_CONSTRAINT_POLICY` | same on macOS; upstream has no iOS path |
/// | Windows | MMCSS (`AvSetMmThreadCharacteristicsW`) | same |
/// | Android | `setpriority` to nice -19 (urgent audio) | upstream has no Android path |
/// | Everything else | no-op reporting success | same |
///
/// Three deviations from upstream are deliberate. The crate owns these
/// numbers, none of them is network-visible (this is host-side scheduling
/// only), and none is worth forking it or hand-writing FFI over.
///
/// 1. **Linux realtime priority.** Upstream asks for `SCHED_FIFO` priority 35;
///    the crate asks for 10. The crate does expose a `set_rt_priority`
///    override, but it writes a *process-global* atomic that is never
///    restored, so calling it from a library would silently re-point every
///    other user of the crate in the same process. It is also absent whenever
///    the crate's `dbus` feature is enabled, and Cargo features are additive:
///    a downstream crate turning `dbus` on would stop this crate compiling.
///    Both are unacceptable for a library, so the default stands.
/// 2. **macOS duty cycle.** Upstream asks for `computation = 0.2 * period`;
///    the crate asks for `period / 2`, tracking macOS 12's own behaviour. Both
///    request the same time-constraint policy.
/// 3. **Windows MMCSS task.** Upstream registers the `Distribution` task and
///    then calls `AvSetMmThreadPriority(.., AVRT_PRIORITY_HIGH)`; the crate
///    registers the `Audio` task and does not raise within it. Both obtain
///    MMCSS scheduling; this port does not reach upstream's boosted tier.
///
pub struct ThreadPriority {
    handle: Option<audio_thread_priority::RtPriorityHandle>,
}

impl core::fmt::Debug for ThreadPriority {
    // `RtPriorityHandle` is opaque and not `Debug`, so report only whether a
    // priority is currently captured.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadPriority")
            .field("raised", &self.handle.is_some())
            .finish()
    }
}

impl Default for ThreadPriority {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadPriority {
    pub fn new() -> Self {
        Self { handle: None }
    }

    /// Raises the calling thread's scheduling priority, capturing the current
    /// parameters so `reset` can restore them. No-op if a priority has already
    /// been captured and not yet reset.
    pub fn set_high(&mut self) {
        if self.handle.is_some() {
            return;
        }

        match audio_thread_priority::promote_current_thread_to_real_time(48, 48_000) {
            Ok(handle) => self.handle = Some(handle),
            Err(e) => {
                tracing::debug!("could not raise Link thread priority: {e}");
            }
        }
    }

    /// Restores the scheduling priority captured by `set_high`. No-op if
    /// nothing was captured.
    pub fn reset(&mut self) {
        if let Some(handle) = self.handle.take() {
            if let Err(e) = audio_thread_priority::demote_current_thread_from_real_time(handle) {
                tracing::debug!("could not restore Link thread priority: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_completely_safe_thread_creation() {
        let result = Arc::new(Mutex::new(None));
        let result_clone = result.clone();

        let handle = ThreadFactory::make_thread("test_thread".to_string(), move || {
            *result_clone.lock().unwrap() = Some(42);
            42
        });

        let thread_result = handle.join().unwrap();
        assert_eq!(thread_result, 42);
        assert_eq!(*result.lock().unwrap(), Some(42));
    }

    #[test]
    fn test_completely_safe_thread_naming() {
        let handle = ThreadFactory::make_thread("named_thread".to_string(), || {
            // Just verify the thread was created successfully
            std::thread::current().name().map(|s| s.to_string())
        });

        let name = handle.join().unwrap();
        // Note: thread name might be truncated on some platforms
        if let Some(actual_name) = name {
            assert!(!actual_name.is_empty());
        }
    }

    #[test]
    fn test_completely_safe_thread_naming_with_special_chars() {
        // Test with a name that contains characters that might cause issues
        let special_name = "test_thread_with_特殊字符_and_números_123".to_string();

        let handle = ThreadFactory::make_thread(special_name, || {
            // Thread should be created successfully even with special characters
            42
        });

        let result = handle.join().unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn test_completely_safe_thread_with_null_bytes() {
        // std::thread::Builder::name() panics on null bytes.
        // Suppress the default panic hook to keep test output clean.
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let name_with_nulls = "test\0thread\0name".to_string();
        let handle_result =
            std::panic::catch_unwind(|| ThreadFactory::make_thread(name_with_nulls, || 100));

        std::panic::set_hook(prev_hook);

        match handle_result {
            Ok(handle) => {
                let result = handle.join().unwrap();
                assert_eq!(result, 100);
            }
            Err(_) => {
                // Panic is expected — null bytes are rejected by std::thread::Builder
            }
        }
    }

    #[test]
    fn test_multiple_completely_safe_threads() {
        let num_threads = 5;
        let results = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for i in 0..num_threads {
            let results_clone = results.clone();
            let handle = ThreadFactory::make_thread(format!("worker_thread_{}", i), move || {
                results_clone.lock().unwrap().push(i);
                i
            });
            handles.push(handle);
        }

        // Wait for all threads to complete
        for handle in handles {
            handle.join().unwrap();
        }

        let final_results = results.lock().unwrap();
        assert_eq!(final_results.len(), num_threads);

        // Check that all thread IDs are present
        for i in 0..num_threads {
            assert!(final_results.contains(&i));
        }
    }

    #[test]
    fn test_thread_name_visibility() {
        // Test that thread names are properly set and visible
        let expected_name = "test_visible_name".to_string();

        let handle = ThreadFactory::make_thread(expected_name, move || {
            // Get the current thread's name
            std::thread::current().name().map(|s| s.to_string())
        });

        let actual_name = handle.join().unwrap();

        // Verify the name was set (may be truncated on some platforms)
        if let Some(name) = actual_name {
            assert!(
                name.starts_with("test_visible"),
                "Thread name should start with 'test_visible', got: {}",
                name
            );
        }
    }

    // Scheduling changes need privileges this test runner may not have, so
    // these assert the state machine, not that the OS honoured the request.

    #[test]
    fn starts_unraised() {
        let p = ThreadPriority::new();
        assert!(p.handle.is_none());
        assert_eq!(format!("{p:?}"), "ThreadPriority { raised: false }");
    }

    #[test]
    fn reset_without_set_high_is_a_noop() {
        let mut p = ThreadPriority::default();
        p.reset();
        assert!(p.handle.is_none());
    }

    #[test]
    fn set_high_is_idempotent_and_reset_clears() {
        let mut p = ThreadPriority::new();
        p.set_high();
        let raised = p.handle.is_some();

        // A second call must not capture a second handle over the first.
        p.set_high();
        assert_eq!(p.handle.is_some(), raised);

        p.reset();
        assert!(p.handle.is_none());

        // And reset must stay a noop once already reset.
        p.reset();
        assert!(p.handle.is_none());
    }
}
