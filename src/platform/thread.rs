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
/// `set_high` captures the thread's current scheduling parameters the first
/// call and is a no-op on subsequent calls until `reset` is called; `reset`
/// restores the captured parameters and is a no-op if nothing was captured.
///
/// The captured state is thread-affine — the parameters belong to whichever
/// thread called `set_high`, and on Windows `AvRevertMmThreadCharacteristics`
/// must run on the thread that created the handle. The type is therefore
/// deliberately `!Send`: it must be constructed, used and dropped on a single
/// thread.
#[derive(Debug, Default)]
pub struct ThreadPriority {
    #[cfg(target_os = "linux")]
    original: Option<(libc::c_int, libc::sched_param)>,
    #[cfg(target_os = "macos")]
    original: Option<mach2::thread_policy::thread_time_constraint_policy_data_t>,
    #[cfg(target_os = "windows")]
    original: Option<winapi::shared::ntdef::HANDLE>,
    /// Pins the captured, thread-affine state to its originating thread by
    /// making the type neither `Send` nor `Sync`.
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Minimal MMCSS (`avrt.dll`) bindings.
///
/// Declared locally rather than imported from `winapi::um::avrt` because that
/// module is gated behind winapi's `avrt` feature, and this workflow may not
/// modify `Cargo.toml`. The signatures match `avrt.h` exactly.
#[cfg(target_os = "windows")]
mod avrt {
    use winapi::shared::minwindef::{BOOL, DWORD, LPDWORD};
    use winapi::shared::ntdef::{HANDLE, LPCWSTR};

    pub const AVRT_PRIORITY_HIGH: DWORD = 1;

    #[link(name = "avrt")]
    extern "system" {
        pub fn AvSetMmThreadCharacteristicsW(TaskName: LPCWSTR, TaskIndex: LPDWORD) -> HANDLE;
        pub fn AvSetMmThreadPriority(AvrtHandle: HANDLE, Priority: DWORD) -> BOOL;
        pub fn AvRevertMmThreadCharacteristics(avrt_handle: HANDLE) -> BOOL;
    }
}

impl ThreadPriority {
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the calling thread's scheduling priority. Captures the
    /// current priority so it can be restored by `reset`. Noop if a
    /// priority has already been captured and not yet reset.
    #[cfg(target_os = "linux")]
    pub fn set_high(&mut self) {
        if self.original.is_none() {
            self.capture_linux();
        }

        // SAFETY: `high` is a valid, fully-initialized `sched_param`, and
        // `pthread_self()` always returns a valid handle for the calling
        // thread.
        unsafe {
            let mut high: libc::sched_param = std::mem::zeroed();
            high.sched_priority = 35;
            libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &high);
        }
    }

    /// Resets the calling thread's scheduling priority to what it was
    /// before `set_high` was called. Noop if no priority was captured.
    #[cfg(target_os = "linux")]
    pub fn reset(&mut self) {
        if let Some((policy, params)) = self.original.take() {
            // SAFETY: `params` was previously captured from a successful
            // `pthread_getschedparam` call and is a valid `sched_param`.
            unsafe {
                libc::pthread_setschedparam(libc::pthread_self(), policy, &params);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn capture_linux(&mut self) {
        // SAFETY: `policy` and `params` are valid out-parameters for
        // `pthread_getschedparam`, and `pthread_self()` always returns a
        // valid handle for the calling thread.
        unsafe {
            let mut policy: libc::c_int = 0;
            let mut params: libc::sched_param = std::mem::zeroed();
            let result =
                libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut params);
            if result == 0 {
                self.original = Some((policy, params));
            }
        }
    }

    /// Raises the calling thread's scheduling priority. Captures the
    /// current priority so it can be restored by `reset`. Noop if a
    /// priority has already been captured and not yet reset.
    #[cfg(target_os = "macos")]
    pub fn set_high(&mut self) {
        use mach2::thread_policy::{
            thread_policy_set, thread_time_constraint_policy_data_t, THREAD_TIME_CONSTRAINT_POLICY,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        };

        if self.original.is_none() {
            self.capture_macos();
        }

        let mut info = std::mem::MaybeUninit::<mach2::mach_time::mach_timebase_info>::uninit();
        // SAFETY: `info` is a valid out-parameter for `mach_timebase_info`.
        let info_result = unsafe { mach2::mach_time::mach_timebase_info(info.as_mut_ptr()) };
        if info_result != mach2::kern_return::KERN_SUCCESS {
            return;
        }
        // SAFETY: `mach_timebase_info` returned KERN_SUCCESS, so `info` is
        // initialized.
        let info = unsafe { info.assume_init() };
        let mach_ratio = info.denom as f64 / info.numer as f64;
        let millisecond = mach_ratio * 1_000_000.0;

        let mut policy = thread_time_constraint_policy_data_t {
            // The nominal time interval between the beginnings of two
            // consecutive duty cycles; how often the thread expects to run.
            period: (millisecond * 1.0) as u32,
            // The amount of CPU time the thread needs during each period.
            computation: (millisecond * 0.2) as u32,
            // The maximum real time that may elapse from the start of a
            // period to the end of computation.
            constraint: (millisecond * 1.0) as u32,
            // The thread's computation can be preempted by other threads.
            preemptible: 1,
        };

        // SAFETY: `mach_thread_self` returns an owned send right to the
        // calling thread's port, `policy` is a valid, fully-initialized
        // `thread_time_constraint_policy_data_t` matching
        // `THREAD_TIME_CONSTRAINT_POLICY_COUNT`, and the send right is
        // released again with `mach_port_deallocate` before returning.
        unsafe {
            let this_thread = mach2::mach_init::mach_thread_self();
            thread_policy_set(
                this_thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut policy as *mut _ as mach2::thread_policy::thread_policy_t,
                THREAD_TIME_CONSTRAINT_POLICY_COUNT,
            );
            mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), this_thread);
        }
    }

    /// Resets the calling thread's scheduling priority to what it was
    /// before `set_high` was called. Noop if no priority was captured.
    #[cfg(target_os = "macos")]
    pub fn reset(&mut self) {
        use mach2::thread_policy::{
            thread_policy_set, THREAD_TIME_CONSTRAINT_POLICY, THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        };

        if let Some(mut policy) = self.original.take() {
            // SAFETY: `mach_thread_self` returns an owned send right to the
            // calling thread's port, `policy` was previously captured from a
            // successful `thread_policy_get` call, and the send right is
            // released again with `mach_port_deallocate` before returning.
            unsafe {
                let this_thread = mach2::mach_init::mach_thread_self();
                thread_policy_set(
                    this_thread,
                    THREAD_TIME_CONSTRAINT_POLICY,
                    &mut policy as *mut _ as mach2::thread_policy::thread_policy_t,
                    THREAD_TIME_CONSTRAINT_POLICY_COUNT,
                );
                mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), this_thread);
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn capture_macos(&mut self) {
        use mach2::thread_policy::{
            thread_policy_get, thread_time_constraint_policy_data_t, THREAD_TIME_CONSTRAINT_POLICY,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        };

        let mut policy = thread_time_constraint_policy_data_t {
            period: 0,
            computation: 0,
            constraint: 0,
            preemptible: 0,
        };
        let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
        let mut get_default = 0;
        // SAFETY: `mach_thread_self` returns an owned send right to the
        // calling thread's port; `policy`, `count` and `get_default` are
        // valid out-parameters for `thread_policy_get`; and the send right is
        // released again with `mach_port_deallocate` before returning.
        let result = unsafe {
            let this_thread = mach2::mach_init::mach_thread_self();
            let result = thread_policy_get(
                this_thread,
                THREAD_TIME_CONSTRAINT_POLICY,
                &mut policy as *mut _ as mach2::thread_policy::thread_policy_t,
                &mut count,
                &mut get_default,
            );
            mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), this_thread);
            result
        };
        if result == mach2::kern_return::KERN_SUCCESS {
            self.original = Some(policy);
        }
    }

    /// Raises the calling thread's scheduling priority using the Windows
    /// Multimedia Class Scheduler Service (MMCSS). Noop if a priority has
    /// already been raised and not yet reset.
    #[cfg(target_os = "windows")]
    pub fn set_high(&mut self) {
        use crate::platform::thread::avrt::{
            AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, AVRT_PRIORITY_HIGH,
        };

        if self.original.is_some() {
            return;
        }

        let name: Vec<u16> = "Distribution\0".encode_utf16().collect();
        let mut task_index: u32 = 0;
        // SAFETY: `name` is a valid null-terminated wide string and
        // `task_index` is a valid out-parameter.
        let handle = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut task_index) };
        if !handle.is_null() {
            // SAFETY: `handle` was just returned by a successful call to
            // `AvSetMmThreadCharacteristicsW`.
            unsafe {
                AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH);
            }
            self.original = Some(handle);
        }
    }

    /// Reverts the thread's MMCSS characteristics set by `set_high`. Noop
    /// if `set_high` did not successfully raise the priority.
    #[cfg(target_os = "windows")]
    pub fn reset(&mut self) {
        use crate::platform::thread::avrt::AvRevertMmThreadCharacteristics;

        if let Some(handle) = self.original.take() {
            // SAFETY: `handle` was previously returned by a successful call
            // to `AvSetMmThreadCharacteristicsW` and has not yet been
            // reverted.
            unsafe {
                AvRevertMmThreadCharacteristics(handle);
            }
        }
    }

    /// Raises the calling thread's scheduling priority. Noop on platforms
    /// without a specific implementation.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub fn set_high(&mut self) {}

    /// Resets the calling thread's scheduling priority. Noop on platforms
    /// without a specific implementation.
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub fn reset(&mut self) {}
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

    #[test]
    fn test_thread_priority_set_high_and_reset_are_noop_safe() {
        // `set_high`/`reset` must not panic regardless of platform support,
        // and calling `reset` without a prior `set_high` must be a noop.
        let mut priority = ThreadPriority::new();
        priority.reset();
        priority.set_high();
        priority.set_high(); // repeated call must be a noop, not an error
        priority.reset();
        priority.reset(); // repeated reset must also be a noop
    }

    #[test]
    fn test_thread_priority_default() {
        let priority = ThreadPriority::default();
        // Should construct without a captured priority.
        let _ = format!("{priority:?}");
    }
}
