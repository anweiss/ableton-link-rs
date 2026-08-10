//! Splits application audio buffers into wire-sized chunks.
//!
//! Ported from `ableton/link_audio/Resizer.hpp`. The resizer accumulates
//! incoming frames until a full message worth of audio is available, tracking
//! tempo changes as separate chunks so that each chunk describes a single
//! constant-tempo run of frames.

use crate::link::{beats::Beats, tempo::Tempo};

use super::payload::{Chunk, Id};

/// Invoked with a complete block of interleaved samples and the chunks that
/// describe it.
pub trait ResizerSink {
    fn flush(
        &mut self,
        samples: &[i16],
        chunks: &[Chunk],
        num_channels: u32,
        sample_rate: u32,
        session_id: Id,
    );
}

impl<F> ResizerSink for F
where
    F: FnMut(&[i16], &[Chunk], u32, u32, Id),
{
    fn flush(
        &mut self,
        samples: &[i16],
        chunks: &[Chunk],
        num_channels: u32,
        sample_rate: u32,
        session_id: Id,
    ) {
        self(samples, chunks, num_channels, sample_rate, session_id)
    }
}

pub struct Resizer<S> {
    successor: S,
    max_num_samples: usize,
    cache: Vec<i16>,
    cached_frames: u32,
    num_channels: u32,
    sample_rate: u32,
    session_id: Id,
    chunks: Vec<Chunk>,
    count: u64,
}

impl<S: ResizerSink> Resizer<S> {
    /// Creates a resizer that emits at most `max_num_bytes` of encoded audio
    /// per message.
    pub fn new(successor: S, max_num_bytes: usize) -> Self {
        let max_num_samples = max_num_bytes / core::mem::size_of::<i16>();
        Resizer {
            successor,
            max_num_samples,
            cache: vec![0; max_num_samples],
            cached_frames: 0,
            num_channels: 0,
            sample_rate: 0,
            session_id: Id::default(),
            chunks: Vec::new(),
            count: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        samples: &[i16],
        num_frames: u32,
        num_channels: u32,
        sample_rate: u32,
        begin_beats: Beats,
        tempo: Tempo,
        session_id: Id,
    ) {
        if num_channels == 0 || num_frames == 0 {
            return;
        }

        if self.cached_frames != 0
            && (num_channels != self.num_channels
                || sample_rate != self.sample_rate
                || session_id != self.session_id)
        {
            self.flush();
        }

        if self.cached_frames == 0 {
            self.sample_rate = sample_rate;
            self.num_channels = num_channels;
            self.session_id = session_id;
            self.chunks.clear();
            self.new_chunk(begin_beats, tempo);
        } else if let Some(last) = self.chunks.last().copied() {
            if tempo != last.tempo && begin_beats != self.chunk_end_beats(&last) {
                self.new_chunk(begin_beats, tempo);
            }
        }

        let frames_per_message = (self.max_num_samples / num_channels as usize) as u32;

        for frame in 0..num_frames {
            for channel in 0..num_channels {
                let src = (num_channels * frame + channel) as usize;
                let dst = (num_channels * self.cached_frames + channel) as usize;
                if src >= samples.len() || dst >= self.cache.len() {
                    break;
                }
                self.cache[dst] = samples[src];
            }
            self.cached_frames += 1;
            if let Some(last) = self.chunks.last_mut() {
                last.num_frames += 1;
            }

            if self.cached_frames >= frames_per_message {
                let next_chunk_begin_beats = self
                    .chunks
                    .last()
                    .map(|c| self.chunk_end_beats(c))
                    .unwrap_or(begin_beats);
                self.flush();

                // Only open a new chunk if there is more audio to place in it.
                if frame + 1 < num_frames {
                    self.new_chunk(next_chunk_begin_beats, tempo);
                }
            }
        }
    }

    /// Emits everything currently cached and resets the chunk state.
    pub fn flush(&mut self) {
        if self.chunks.is_empty() {
            self.cached_frames = 0;
            return;
        }

        let num_samples = self.cached_frames as usize * self.num_channels as usize;
        self.successor.flush(
            &self.cache[..num_samples.min(self.cache.len())],
            &self.chunks,
            self.num_channels,
            self.sample_rate,
            self.session_id,
        );
        self.cached_frames = 0;
        self.chunks.clear();
    }

    fn chunk_end_beats(&self, chunk: &Chunk) -> Beats {
        if self.sample_rate == 0 || chunk.tempo.bpm() == 0.0 {
            return chunk.begin_beats;
        }
        let seconds_per_beat = 60.0 / chunk.tempo.bpm();
        let range_duration = chunk.num_frames as f64 / self.sample_rate as f64;
        chunk.begin_beats + Beats::new(range_duration / seconds_per_beat)
    }

    fn new_chunk(&mut self, beats: Beats, tempo: Tempo) {
        self.count += 1;
        self.chunks.push(Chunk {
            count: self.count,
            num_frames: 0,
            begin_beats: beats,
            tempo,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct Captured {
        samples: Vec<i16>,
        chunks: Vec<Chunk>,
        num_channels: u32,
        sample_rate: u32,
    }

    fn collector() -> (Arc<Mutex<Vec<Captured>>>, impl ResizerSink) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink_captured = captured.clone();
        let sink = move |samples: &[i16],
                         chunks: &[Chunk],
                         num_channels: u32,
                         sample_rate: u32,
                         _session: Id| {
            sink_captured.lock().unwrap().push(Captured {
                samples: samples.to_vec(),
                chunks: chunks.to_vec(),
                num_channels,
                sample_rate,
            });
        };
        (captured, sink)
    }

    #[test]
    fn emits_when_the_message_is_full() {
        let (captured, sink) = collector();
        // 8 bytes => 4 samples => 2 stereo frames per message.
        let mut resizer = Resizer::new(sink, 8);

        let samples: Vec<i16> = (0..8).collect();
        resizer.process(
            &samples,
            4,
            2,
            44100,
            Beats::new(0.0),
            Tempo::new(120.0),
            Id::default(),
        );

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].samples, vec![0, 1, 2, 3]);
        assert_eq!(captured[1].samples, vec![4, 5, 6, 7]);
        assert_eq!(captured[0].num_channels, 2);
        assert_eq!(captured[0].sample_rate, 44100);
        assert_eq!(captured[0].chunks[0].num_frames, 2);
        assert_eq!(captured[0].chunks[0].count, 1);
        assert_eq!(captured[1].chunks[0].count, 2);
    }

    #[test]
    fn partial_buffers_are_cached_until_flushed() {
        let (captured, sink) = collector();
        let mut resizer = Resizer::new(sink, 16);

        resizer.process(
            &[1, 2],
            1,
            2,
            44100,
            Beats::new(0.0),
            Tempo::new(120.0),
            Id::default(),
        );
        assert!(captured.lock().unwrap().is_empty());

        resizer.flush();
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].samples, vec![1, 2]);
    }

    #[test]
    fn format_change_flushes_previous_audio() {
        let (captured, sink) = collector();
        let mut resizer = Resizer::new(sink, 64);

        resizer.process(
            &[1, 2],
            1,
            2,
            44100,
            Beats::new(0.0),
            Tempo::new(120.0),
            Id::default(),
        );
        resizer.process(
            &[3],
            1,
            1,
            48000,
            Beats::new(1.0),
            Tempo::new(120.0),
            Id::default(),
        );

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].num_channels, 2);
        assert_eq!(captured[0].sample_rate, 44100);
    }

    #[test]
    fn tempo_change_opens_a_new_chunk() {
        let (captured, sink) = collector();
        let mut resizer = Resizer::new(sink, 64);

        resizer.process(
            &[1, 2],
            1,
            2,
            44100,
            Beats::new(0.0),
            Tempo::new(120.0),
            Id::default(),
        );
        resizer.process(
            &[3, 4],
            1,
            2,
            44100,
            Beats::new(10.0),
            Tempo::new(140.0),
            Id::default(),
        );
        resizer.flush();

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].chunks.len(), 2);
        assert_eq!(captured[0].chunks[1].tempo, Tempo::new(140.0));
        assert_eq!(captured[0].chunks[1].begin_beats, Beats::new(10.0));
    }

    #[test]
    fn zero_length_input_is_ignored() {
        let (captured, sink) = collector();
        let mut resizer = Resizer::new(sink, 64);
        resizer.process(
            &[],
            0,
            2,
            44100,
            Beats::new(0.0),
            Tempo::new(120.0),
            Id::default(),
        );
        assert!(captured.lock().unwrap().is_empty());
    }
}
