//! A single-producer / single-consumer queue of pre-allocated audio slots.
//!
//! Ported from `ableton/link_audio/Queue.hpp`. Upstream implements this as a
//! lock-free ring of raw slots; this port keeps the same ownership model — a
//! fixed pool of buffers recycled between the writer and the reader, so that
//! neither side allocates while audio is flowing — but expresses it in safe
//! Rust by moving buffer ownership through bounded channels instead of handing
//! out pointers into a shared ring.
//!
//! The writer retains a slot, fills it, and releases it to the reader. The
//! reader retains it, consumes it, and releases it back into the free pool.

use tokio::sync::mpsc::{
    channel,
    error::{TryRecvError, TrySendError},
    Receiver, Sender,
};

/// The producing half of the queue.
pub struct Writer<T> {
    free_rx: Receiver<T>,
    filled_tx: Sender<T>,
    current: Option<T>,
    num_slots: usize,
}

/// The consuming half of the queue.
pub struct Reader<T> {
    filled_rx: Receiver<T>,
    free_tx: Sender<T>,
    current: Option<T>,
    num_slots: usize,
}

/// Creates a queue with `num_slots` pre-allocated slots, each initialized by
/// cloning `value`.
pub fn queue<T: Clone>(num_slots: usize, value: T) -> (Writer<T>, Reader<T>) {
    let num_slots = num_slots.max(1);
    let (free_tx, free_rx) = channel(num_slots);
    let (filled_tx, filled_rx) = channel(num_slots);

    for _ in 0..num_slots {
        // The channel was created with exactly this capacity.
        let _ = free_tx.try_send(value.clone());
    }

    (
        Writer {
            free_rx,
            filled_tx,
            current: None,
            num_slots,
        },
        Reader {
            filled_rx,
            free_tx,
            current: None,
            num_slots,
        },
    )
}

impl<T> Writer<T> {
    /// Takes ownership of a free slot. Returns `false` when no slot is
    /// available, i.e. the reader has not kept up, or when a slot is already
    /// retained.
    pub fn retain_slot(&mut self) -> bool {
        if self.current.is_some() {
            return false;
        }
        match self.free_rx.try_recv() {
            Ok(slot) => {
                self.current = Some(slot);
                true
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => false,
        }
    }

    /// Hands the retained slot to the reader. Does nothing if no slot is
    /// currently retained.
    pub fn release_slot(&mut self) {
        if let Some(slot) = self.current.take() {
            if let Err(TrySendError::Closed(_) | TrySendError::Full(_)) =
                self.filled_tx.try_send(slot)
            {
                // The reader is gone or saturated; dropping the slot only
                // shrinks the pool for the lifetime of this queue.
            }
        }
    }

    /// The slot currently retained by the writer, if any.
    pub fn slot_mut(&mut self) -> Option<&mut T> {
        self.current.as_mut()
    }

    pub fn num_retained_slots(&self) -> usize {
        usize::from(self.current.is_some())
    }

    pub fn num_slots(&self) -> usize {
        self.num_slots
    }
}

impl<T> Reader<T> {
    /// Takes ownership of the next slot handed over by the writer. Returns
    /// `false` when no filled slot is available.
    pub fn retain_slot(&mut self) -> bool {
        if self.current.is_some() {
            return false;
        }
        match self.filled_rx.try_recv() {
            Ok(slot) => {
                self.current = Some(slot);
                true
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => false,
        }
    }

    /// Returns the retained slot to the free pool.
    pub fn release_slot(&mut self) {
        if let Some(slot) = self.current.take() {
            let _ = self.free_tx.try_send(slot);
        }
    }

    /// The slot currently retained by the reader, if any.
    pub fn slot_mut(&mut self) -> Option<&mut T> {
        self.current.as_mut()
    }

    pub fn num_retained_slots(&self) -> usize {
        usize::from(self.current.is_some())
    }

    pub fn num_slots(&self) -> usize {
        self.num_slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_travel_from_writer_to_reader() {
        let (mut writer, mut reader) = queue(4, 0i32);

        assert_eq!(writer.num_slots(), 4);
        assert!(!reader.retain_slot());

        assert!(writer.retain_slot());
        *writer.slot_mut().unwrap() = 42;
        writer.release_slot();

        assert!(reader.retain_slot());
        assert_eq!(*reader.slot_mut().unwrap(), 42);
        reader.release_slot();

        assert!(!reader.retain_slot());
    }

    #[test]
    fn only_one_slot_is_retained_at_a_time() {
        let (mut writer, _reader) = queue(2, 0u8);
        assert!(writer.retain_slot());
        assert!(!writer.retain_slot());
        assert_eq!(writer.num_retained_slots(), 1);
    }

    #[test]
    fn queue_becomes_full_when_the_reader_stalls() {
        let (mut writer, _reader) = queue(2, 0u8);

        assert!(writer.retain_slot());
        writer.release_slot();
        assert!(writer.retain_slot());
        writer.release_slot();
        // The pool is exhausted until the reader recycles a slot.
        assert!(!writer.retain_slot());
    }

    #[test]
    fn slots_are_recycled() {
        let (mut writer, mut reader) = queue(2, 0usize);

        for i in 0..10 {
            assert!(writer.retain_slot());
            *writer.slot_mut().unwrap() = i;
            writer.release_slot();

            assert!(reader.retain_slot());
            assert_eq!(*reader.slot_mut().unwrap(), i);
            reader.release_slot();
        }
    }

    #[test]
    fn slot_mut_is_none_when_nothing_is_retained() {
        let (mut writer, mut reader) = queue(2, 0u8);
        assert!(writer.slot_mut().is_none());
        assert!(reader.slot_mut().is_none());
    }

    #[test]
    fn works_across_threads() {
        let (mut writer, mut reader) = queue(8, 0usize);

        let producer = std::thread::spawn(move || {
            let mut sent = 0;
            while sent < 1000 {
                if writer.retain_slot() {
                    *writer.slot_mut().unwrap() = sent;
                    writer.release_slot();
                    sent += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });

        let mut received = 0;
        while received < 1000 {
            if reader.retain_slot() {
                assert_eq!(*reader.slot_mut().unwrap(), received);
                reader.release_slot();
                received += 1;
            } else {
                std::thread::yield_now();
            }
        }

        producer.join().unwrap();
    }
}
