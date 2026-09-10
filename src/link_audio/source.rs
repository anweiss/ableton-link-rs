//! A subscription to an audio channel published by another peer.
//!
//! Ported from `ableton/link_audio/Source.hpp`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use super::{buffer::BufferCallbackHandle, payload::Id};

/// Invoked on a Link-managed thread whenever a buffer is received.
pub type SourceCallback = Box<dyn FnMut(BufferCallbackHandle<'_>) + Send + 'static>;

pub struct Source {
    id: Id,
    callback: Mutex<SourceCallback>,
    closed: Option<Arc<AtomicBool>>,
}

impl Source {
    pub fn new(id: Id, callback: SourceCallback) -> Self {
        Source {
            id,
            callback: Mutex::new(callback),
            closed: None,
        }
    }

    pub(super) fn new_managed(id: Id, callback: SourceCallback, closed: Arc<AtomicBool>) -> Self {
        Self {
            id,
            callback: Mutex::new(callback),
            closed: Some(closed),
        }
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn set_callback(&self, callback: SourceCallback) {
        match self.callback.lock() {
            Ok(mut current) => *current = callback,
            Err(poisoned) => *poisoned.into_inner() = callback,
        }
    }

    pub fn invoke(&self, buffer: BufferCallbackHandle<'_>) {
        let mut callback = self
            .callback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self
            .closed
            .as_ref()
            .is_some_and(|closed| closed.load(Ordering::Acquire))
        {
            callback(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link_audio::buffer::BufferInfo;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn info() -> BufferInfo {
        BufferInfo {
            num_channels: 2,
            num_frames: 1,
            sample_rate: 44100,
            count: 1,
            session_beat_time: 0.0,
            tempo: 120.0,
            session_id: Id::default(),
        }
    }

    #[test]
    fn callbacks_receive_buffers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let source = Source::new(
            Id::from_array([1; 8]),
            Box::new(move |handle| {
                assert_eq!(handle.samples, &[1, 2]);
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );

        assert_eq!(source.id(), Id::from_array([1; 8]));
        source.invoke(BufferCallbackHandle {
            samples: &[1, 2],
            info: info(),
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callbacks_can_be_replaced() {
        let source = Source::new(Id::default(), Box::new(|_| panic!("original callback")));
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        source.set_callback(Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        source.invoke(BufferCallbackHandle {
            samples: &[],
            info: info(),
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn managed_callback_replacement_cannot_bypass_shutdown() {
        let closed = Arc::new(AtomicBool::new(false));
        let source = Source::new_managed(Id::default(), Box::new(|_| {}), closed.clone());
        closed.store(true, Ordering::Release);
        source.set_callback(Box::new(|_| panic!("callback admitted after shutdown")));
        source.invoke(BufferCallbackHandle {
            samples: &[],
            info: info(),
        });
    }
}
