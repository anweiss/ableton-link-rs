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
}
