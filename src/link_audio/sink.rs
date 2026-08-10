//! An audio channel published to the Link session.
//!
//! Ported from `ableton/link_audio/Sink.hpp`. A sink owns the writer side of a
//! pre-allocated buffer pool; the application fills a buffer and commits it,
//! and the sink processor encodes and sends it to every peer that has
//! requested the channel.

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex, MutexGuard,
};

use crate::link::{beats::Beats, node::NodeId, timeline::Timeline};

use super::{
    beat_time_mapping::global_beat_at_beat,
    buffer::Buffer,
    payload::{truncate_name, Id},
    queue::{self, Reader, Writer},
};

/// Number of buffers in a sink's pool. Matches upstream's `Queue` size.
pub const SINK_QUEUE_SIZE: usize = 128;

pub struct Sink {
    name: Mutex<String>,
    name_is_up_to_date: AtomicBool,
    id: Id,
    max_num_samples: AtomicUsize,
    writer: Mutex<Writer<Buffer>>,
    is_connected: AtomicBool,
}

impl Sink {
    /// Creates a sink and the reader half of its buffer pool. The reader is
    /// owned by the sink processor on the Link thread.
    pub fn new(name: impl Into<String>, max_num_samples: usize, id: Id) -> (Self, Reader<Buffer>) {
        let (writer, reader) = queue::queue(SINK_QUEUE_SIZE, Buffer::new(max_num_samples));
        (
            Sink {
                name: Mutex::new(truncate_name(&name.into())),
                name_is_up_to_date: AtomicBool::new(false),
                id,
                max_num_samples: AtomicUsize::new(max_num_samples),
                writer: Mutex::new(writer),
                is_connected: AtomicBool::new(false),
            },
            reader,
        )
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn name(&self) -> String {
        self.name
            .lock()
            .map(|n| n.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    pub fn set_name(&self, name: impl Into<String>) {
        let name = truncate_name(&name.into());
        if let Ok(mut current) = self.name.lock() {
            *current = name;
        }
        self.name_is_up_to_date.store(false, Ordering::Release);
    }

    /// Returns `true` once after every name change, mirroring upstream's
    /// `std::atomic_flag::test_and_set` usage.
    pub fn name_changed(&self) -> bool {
        !self.name_is_up_to_date.swap(true, Ordering::AcqRel)
    }

    /// Requests a larger buffer size for future buffers. Shrinking is a no-op.
    pub fn request_max_num_samples(&self, num_samples: usize) {
        self.max_num_samples
            .fetch_max(num_samples, Ordering::AcqRel);
    }

    pub fn max_num_samples(&self) -> usize {
        self.max_num_samples.load(Ordering::Acquire)
    }

    pub fn set_is_connected(&self, is_connected: bool) {
        self.is_connected.store(is_connected, Ordering::Release);
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Acquire)
    }

    /// Retains a buffer for writing. Returns `None` if no source is listening
    /// or no buffer is available.
    pub fn retain_buffer(&self) -> Option<SinkBufferHandle<'_>> {
        if !self.is_connected() {
            return None;
        }

        let mut writer = self.writer.lock().ok()?;
        if writer.num_retained_slots() > 0 || !writer.retain_slot() {
            return None;
        }

        // Grow buffers that were allocated before a larger size was requested.
        let max_num_samples = self.max_num_samples();
        if let Some(buffer) = writer.slot_mut() {
            if buffer.samples.len() < max_num_samples {
                buffer.samples.resize(max_num_samples, 0);
            }
        }

        Some(SinkBufferHandle {
            sink: self,
            writer,
            committed: false,
        })
    }
}

/// A retained sink buffer. Dropping the handle without calling
/// [`SinkBufferHandle::commit`] discards the audio.
pub struct SinkBufferHandle<'a> {
    sink: &'a Sink,
    writer: MutexGuard<'a, Writer<Buffer>>,
    committed: bool,
}

impl SinkBufferHandle<'_> {
    /// The samples to write into, interleaved by channel.
    pub fn samples_mut(&mut self) -> &mut [i16] {
        self.writer
            .slot_mut()
            .map(|b| b.samples.as_mut_slice())
            .unwrap_or(&mut [])
    }

    pub fn max_num_samples(&self) -> usize {
        self.sink.max_num_samples()
    }

    /// Commits the written audio, tagging it with the timing information
    /// needed to place it on the session beat grid.
    ///
    /// `num_frames * num_channels` must not exceed
    /// [`SinkBufferHandle::max_num_samples`].
    #[allow(clippy::too_many_arguments)]
    pub fn commit(
        mut self,
        timeline: &Timeline,
        session_id: NodeId,
        beats_at_buffer_begin: f64,
        quantum: f64,
        num_frames: usize,
        num_channels: usize,
        sample_rate: u32,
    ) -> bool {
        let max_num_samples = self.sink.max_num_samples();
        if num_channels == 0 || num_frames * num_channels > max_num_samples {
            return false;
        }

        let begin_beats = global_beat_at_beat(
            timeline,
            Beats::new(beats_at_buffer_begin),
            Beats::new(quantum),
        );

        if let Some(buffer) = self.writer.slot_mut() {
            buffer.begin_beats = begin_beats;
            buffer.tempo = timeline.tempo;
            buffer.num_channels = num_channels as u32;
            buffer.sample_rate = sample_rate;
            buffer.num_frames = num_frames as u32;
            buffer.session_id = session_id;
        } else {
            return false;
        }

        self.writer.release_slot();
        self.committed = true;
        true
    }
}

impl Drop for SinkBufferHandle<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Release without timing information so the processor skips the buffer.
        if let Some(buffer) = self.writer.slot_mut() {
            buffer.clear_timing();
        }
        self.writer.release_slot();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::tempo::Tempo;
    use chrono::Duration;

    fn timeline() -> Timeline {
        Timeline {
            tempo: Tempo::new(120.0),
            beat_origin: Beats::new(0.0),
            time_origin: Duration::zero(),
        }
    }

    #[test]
    fn disconnected_sinks_do_not_hand_out_buffers() {
        let (sink, _reader) = Sink::new("drums", 64, Id::default());
        assert!(sink.retain_buffer().is_none());
    }

    #[test]
    fn committed_buffers_reach_the_reader() {
        let (sink, mut reader) = Sink::new("drums", 8, Id::from_array([7; 8]));
        sink.set_is_connected(true);

        {
            let mut handle = sink.retain_buffer().unwrap();
            handle.samples_mut()[..4].copy_from_slice(&[1, 2, 3, 4]);
            assert!(handle.commit(
                &timeline(),
                NodeId::from_array([9; 8]),
                1.0,
                4.0,
                2,
                2,
                44100
            ));
        }

        assert!(reader.retain_slot());
        let buffer = reader.slot_mut().unwrap();
        assert_eq!(&buffer.samples[..4], &[1, 2, 3, 4]);
        assert_eq!(buffer.num_frames, 2);
        assert_eq!(buffer.num_channels, 2);
        assert_eq!(buffer.sample_rate, 44100);
        assert_eq!(buffer.tempo, Tempo::new(120.0));
        assert_eq!(buffer.session_id, NodeId::from_array([9; 8]));
    }

    #[test]
    fn dropped_buffers_are_marked_as_carrying_no_audio() {
        let (sink, mut reader) = Sink::new("drums", 8, Id::default());
        sink.set_is_connected(true);

        drop(sink.retain_buffer().unwrap());

        assert!(reader.retain_slot());
        assert_eq!(reader.slot_mut().unwrap().tempo, Tempo::new(0.0));
    }

    #[test]
    fn commit_rejects_oversized_buffers() {
        let (sink, _reader) = Sink::new("drums", 8, Id::default());
        sink.set_is_connected(true);
        let handle = sink.retain_buffer().unwrap();
        assert!(!handle.commit(&timeline(), NodeId::default(), 0.0, 4.0, 8, 2, 44100));
    }

    #[test]
    fn only_one_buffer_can_be_retained() {
        let (sink, _reader) = Sink::new("drums", 8, Id::default());
        sink.set_is_connected(true);
        let _handle = sink.retain_buffer().unwrap();
        // The writer lock is held by the outstanding handle.
        assert!(sink.writer.try_lock().is_err());
    }

    #[test]
    fn name_changes_are_reported_once() {
        let (sink, _reader) = Sink::new("drums", 8, Id::default());
        assert!(sink.name_changed());
        assert!(!sink.name_changed());
        sink.set_name("bass");
        assert_eq!(sink.name(), "bass");
        assert!(sink.name_changed());
        assert!(!sink.name_changed());
    }

    #[test]
    fn max_num_samples_only_grows() {
        let (sink, _reader) = Sink::new("drums", 8, Id::default());
        sink.request_max_num_samples(4);
        assert_eq!(sink.max_num_samples(), 8);
        sink.request_max_num_samples(32);
        assert_eq!(sink.max_num_samples(), 32);
    }

    #[test]
    fn names_are_truncated() {
        let (sink, _reader) = Sink::new("x".repeat(300), 8, Id::default());
        assert_eq!(sink.name().len(), 256);
    }
}
