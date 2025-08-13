use std::sync::atomic::{AtomicU32, Ordering};

/// A lock-free triple buffer implementation for real-time safe data exchange
/// between threads. This allows one thread to write data while another reads,
/// without blocking either operation.
pub struct TripleBuffer<T> {
    buffers: [T; 3],
    state: AtomicU32,
    read_index: u32,
    write_index: u32,
}

impl<T: Clone + Default> TripleBuffer<T> {
    pub fn new() -> Self {
        Self {
            buffers: [T::default(), T::default(), T::default()],
            state: AtomicU32::new(Self::make_state(1, false)),
            read_index: 0,
            write_index: 2,
        }
    }

    pub fn new_with_initial(initial: T) -> Self {
        Self {
            buffers: [initial.clone(), initial.clone(), initial],
            state: AtomicU32::new(Self::make_state(1, false)),
            read_index: 0,
            write_index: 2,
        }
    }

    /// Read the current value. This is lock-free and real-time safe.
    pub fn read(&mut self) -> T {
        self.load_read_buffer();
        self.buffers[self.read_index as usize].clone()
    }

    /// Read a new value if one is available. Returns None if no new data.
    /// This is lock-free and real-time safe.
    pub fn read_new(&mut self) -> Option<T> {
        if self.load_read_buffer() {
            Some(self.buffers[self.read_index as usize].clone())
        } else {
            None
        }
    }

    /// Write a new value. This is lock-free and real-time safe.
    pub fn write(&mut self, value: T) {
        self.buffers[self.write_index as usize] = value;

        let prev_state = self
            .state
            .swap(Self::make_state(self.write_index, true), Ordering::AcqRel);

        self.write_index = Self::back_index(prev_state);
    }

    fn load_read_buffer(&mut self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        let is_new = Self::is_new_write(state);

        if is_new {
            let prev_state = self
                .state
                .swap(Self::make_state(self.read_index, false), Ordering::AcqRel);

            self.read_index = Self::back_index(prev_state);
        }

        is_new
    }

    const fn is_new_write(state: u32) -> bool {
        (state & 0x0000FFFF) != 0
    }

    const fn back_index(state: u32) -> u32 {
        state >> 16
    }

    const fn make_state(back_buffer_index: u32, is_write: bool) -> u32 {
        (back_buffer_index << 16) | if is_write { 1 } else { 0 }
    }
}

impl<T: Clone + Default> Default for TripleBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_triple_buffer_basic() {
        let mut buffer = TripleBuffer::new_with_initial(42i32);

        // Initial read should return the initial value
        assert_eq!(buffer.read(), 42);

        // Write a new value
        buffer.write(123);

        // Read should return the new value
        assert_eq!(buffer.read(), 123);
    }

    #[test]
    fn test_triple_buffer_new_detection() {
        let mut buffer = TripleBuffer::new_with_initial(0i32);

        // Initially no new data
        assert_eq!(buffer.read_new(), None);

        // Write data
        buffer.write(42);

        // Should detect new data
        assert_eq!(buffer.read_new(), Some(42));

        // Second read should not detect new data
        assert_eq!(buffer.read_new(), None);
    }

    #[test]
    fn test_triple_buffer_concurrent() {
        let buffer = Arc::new(std::sync::Mutex::new(TripleBuffer::new_with_initial(0i32)));

        let writer_buffer = buffer.clone();
        let writer = thread::spawn(move || {
            for i in 1..=10 {
                {
                    let mut buf = writer_buffer.lock().unwrap();
                    buf.write(i);
                }
                thread::sleep(Duration::from_millis(10));
            }
        });

        let reader_buffer = buffer.clone();
        let reader = thread::spawn(move || {
            let mut last_value = 0;
            for _ in 0..20 {
                {
                    let mut buf = reader_buffer.lock().unwrap();
                    if let Some(value) = buf.read_new() {
                        assert!(value > last_value);
                        last_value = value;
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();
    }
}
