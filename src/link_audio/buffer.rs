//! Audio buffers exchanged between the application and the LinkAudio engine.
//!
//! Ported from `ableton/link_audio/Buffer.hpp`.

use crate::link::{beats::Beats, tempo::Tempo};

use super::payload::Id;

/// A block of interleaved 16-bit samples together with the Link timing
/// information needed to place it on the session beat grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Buffer {
    pub sample_rate: u32,
    pub num_channels: u32,
    pub num_frames: u32,
    pub samples: Vec<i16>,
    pub begin_beats: Beats,
    pub tempo: Tempo,
    pub count: u64,
    pub session_id: Id,
}

impl Buffer {
    pub fn new(num_samples: usize) -> Self {
        Self {
            sample_rate: 0,
            num_channels: 0,
            num_frames: 0,
            samples: vec![0; num_samples],
            begin_beats: Beats::new(0.0),
            tempo: Tempo::new(0.0),
            count: 0,
            session_id: Id::default(),
        }
    }

    /// Number of samples that carry audio, i.e. `num_frames * num_channels`.
    pub fn num_samples(&self) -> usize {
        self.num_frames as usize * self.num_channels as usize
    }

    /// Resets the timing metadata, marking the buffer as carrying no audio.
    pub fn clear_timing(&mut self) {
        self.begin_beats = Beats::new(0.0);
        self.tempo = Tempo::new(0.0);
    }
}

/// Metadata describing a decoded buffer handed to a source callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BufferInfo {
    pub num_channels: usize,
    pub num_frames: usize,
    pub sample_rate: u32,
    pub count: u64,
    /// Beat time at the beginning of the buffer, expressed on the sending
    /// peer's session-global beat grid. Use
    /// [`crate::link_audio::beat_time_mapping::beat_at_global_beat`] (or
    /// [`BufferInfo::begin_beats`]) to map it to local beat time.
    pub session_beat_time: f64,
    pub tempo: f64,
    pub session_id: Id,
}

/// A borrowed view of received audio samples plus their metadata.
#[derive(Debug)]
pub struct BufferCallbackHandle<'a> {
    pub samples: &'a [i16],
    pub info: BufferInfo,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer_is_zeroed() {
        let buffer = Buffer::new(8);
        assert_eq!(buffer.samples, vec![0; 8]);
        assert_eq!(buffer.num_samples(), 0);
    }

    #[test]
    fn clear_timing_resets_tempo_and_beats() {
        let mut buffer = Buffer::new(4);
        buffer.tempo = Tempo::new(120.0);
        buffer.begin_beats = Beats::new(4.0);
        buffer.clear_timing();
        assert_eq!(buffer.tempo, Tempo::new(0.0));
        assert_eq!(buffer.begin_beats, Beats::new(0.0));
    }
}
