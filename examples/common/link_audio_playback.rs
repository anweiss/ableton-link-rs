//! Bounded, nonblocking speaker handoff for the LinkAudio example.

use std::{
    sync::{
        atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use ableton_link_rs::link_audio::AudioBuffer;
use rodio::{ChannelCount, Sample, SampleRate, Source};
use rtrb::{Consumer, Producer, RingBuffer};

const MAX_CHANNELS: usize = 32;
const MAX_SAMPLE_RATE: u32 = 192_000;
const FADE_MS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    channels: ChannelCount,
    rate: SampleRate,
}

impl Format {
    pub fn new(channels: usize, rate: u32) -> Option<Self> {
        if channels > MAX_CHANNELS || rate > MAX_SAMPLE_RATE {
            return None;
        }
        Some(Self {
            channels: ChannelCount::new(u16::try_from(channels).ok()?)?,
            rate: SampleRate::new(rate)?,
        })
    }

    pub fn from_initial_buffer(
        channels: usize,
        rate: u32,
        frames: usize,
        samples: &[i16],
        rejected: &AtomicU64,
    ) -> Option<Format> {
        let format = Format::new(channels, rate)
            .filter(|_| frames > 0 && frames.checked_mul(channels) == Some(samples.len()));
        if format.is_none() {
            rejected.fetch_add(1, Ordering::Relaxed);
        }
        format
    }

    fn channels(self) -> usize {
        self.channels.get() as usize
    }

    fn frames_for_ms(self, ms: usize) -> usize {
        (self.rate.get() as usize * ms).div_ceil(1000).max(1)
    }
}

#[derive(Clone, Copy)]
struct Frame {
    samples: [i16; MAX_CHANNELS],
    generation: u64,
}

pub struct PlaybackStats {
    format: Format,
    prefill_frames: usize,
    buffered_frames: AtomicUsize,
    underrun_events: AtomicU64,
    missing_frames: AtomicU64,
    startup_frames: AtomicU64,
    dropped_frames: AtomicU64,
    rejected_buffers: Arc<AtomicU64>,
    peak: AtomicI32,
}

impl PlaybackStats {
    pub fn prefill_ms(&self) -> f64 {
        self.prefill_frames as f64 * 1000.0 / self.format.rate.get() as f64
    }

    pub fn status(&self) -> String {
        let buffered_ms = self.buffered_frames.load(Ordering::Relaxed) as f64 * 1000.0
            / self.format.rate.get() as f64;
        format!(
            "buffered: {buffered_ms:.1} ms | effective prefill: {:.1} ms | underrun events: {} | missing frames: {} | startup frames: {} | dropped frames: {} | rejected buffers: {} | peak: {:.1}%",
            self.prefill_ms(),
            self.underrun_events.load(Ordering::Relaxed),
            self.missing_frames.load(Ordering::Relaxed),
            self.startup_frames.load(Ordering::Relaxed),
            self.dropped_frames.load(Ordering::Relaxed),
            self.rejected_buffers.load(Ordering::Relaxed),
            self.peak.swap(0, Ordering::Relaxed) as f32 * 100.0 / -(i16::MIN as f32),
        )
    }
}

pub struct PlaybackInput {
    producer: Producer<Frame>,
    format: Format,
    generation: u64,
    stats: Arc<PlaybackStats>,
}

impl PlaybackInput {
    pub fn push(&mut self, channels: usize, rate: u32, frames: usize, samples: &[i16]) {
        if Format::new(channels, rate) != Some(self.format)
            || frames.checked_mul(channels) != Some(samples.len())
        {
            self.stats.rejected_buffers.fetch_add(1, Ordering::Relaxed);
            self.generation = self.generation.wrapping_add(1);
            return;
        }
        let peak = samples
            .iter()
            .map(|sample| (*sample as i32).abs())
            .max()
            .unwrap_or(0);
        self.stats.peak.fetch_max(peak, Ordering::Relaxed);
        let accepted = frames.min(self.producer.slots());
        let mut written = 0;
        for samples in samples.chunks_exact(channels).take(accepted) {
            let mut frame = Frame {
                samples: [0; MAX_CHANNELS],
                generation: self.generation,
            };
            frame.samples[..channels].copy_from_slice(samples);
            // Account before publishing so a concurrent pop cannot underflow.
            self.stats.buffered_frames.fetch_add(1, Ordering::Relaxed);
            if self.producer.push(frame).is_err() {
                self.stats.buffered_frames.fetch_sub(1, Ordering::Relaxed);
                break;
            }
            written += 1;
        }
        if written < frames {
            self.stats
                .dropped_frames
                .fetch_add((frames - written) as u64, Ordering::Relaxed);
            // Mark the next accepted frame, not the still-contiguous queued tail.
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

pub struct LivePlayback {
    consumer: Consumer<Frame>,
    format: Format,
    target_frames: usize,
    fade_frames: usize,
    fade_remaining: usize,
    fade_from: [Sample; MAX_CHANNELS],
    output: [Sample; MAX_CHANNELS],
    channel: usize,
    generation: Option<u64>,
    waiting: bool,
    started: bool,
    pub stats: Arc<PlaybackStats>,
}

impl LivePlayback {
    pub fn new(
        format: Format,
        latency_ms: usize,
        output_burst_ms: usize,
        rejected_buffers: Arc<AtomicU64>,
    ) -> (PlaybackInput, Self) {
        let target_frames = format
            .frames_for_ms(latency_ms.clamp(1, 1000))
            .max(format.frames_for_ms(output_burst_ms.min(1000)));
        let max_chunk_frames = AudioBuffer::MAX_AUDIO_BYTES / size_of::<i16>() / format.channels();
        // A waiting renderer may already hold target - 1 frames when a full
        // wire chunk arrives. Keep that chunk without forcing a partial drop.
        let capacity = (target_frames * 2).max(target_frames - 1 + max_chunk_frames);
        let (producer, consumer) = RingBuffer::new(capacity);
        let stats = Arc::new(PlaybackStats {
            format,
            prefill_frames: target_frames,
            buffered_frames: AtomicUsize::new(0),
            underrun_events: AtomicU64::new(0),
            missing_frames: AtomicU64::new(0),
            startup_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            rejected_buffers,
            peak: AtomicI32::new(0),
        });
        (
            PlaybackInput {
                producer,
                format,
                generation: 0,
                stats: stats.clone(),
            },
            Self {
                consumer,
                format,
                target_frames,
                fade_frames: format.frames_for_ms(FADE_MS),
                fade_remaining: 0,
                fade_from: [0.0; MAX_CHANNELS],
                output: [0.0; MAX_CHANNELS],
                channel: 0,
                generation: None,
                waiting: true,
                started: false,
                stats,
            },
        )
    }

    fn start_fade(&mut self) {
        self.fade_from = self.output;
        self.fade_remaining = self.fade_frames;
    }

    fn render_frame(&mut self) {
        let available = self.consumer.slots();
        if self.waiting && self.fade_remaining == 0 && available >= self.target_frames {
            self.waiting = false;
            self.started = true;
            self.start_fade();
        }

        let frame = if self.waiting {
            None
        } else {
            self.consumer.pop().ok()
        };
        if frame.is_none() && !self.waiting {
            self.waiting = true;
            self.stats.underrun_events.fetch_add(1, Ordering::Relaxed);
            self.start_fade();
        }
        if let Some(frame) = frame {
            self.stats.buffered_frames.fetch_sub(1, Ordering::Relaxed);
            if self.generation.is_some_and(|g| g != frame.generation) {
                self.start_fade();
            }
            self.generation = Some(frame.generation);
            for (output, sample) in self.output[..self.format.channels()]
                .iter_mut()
                .zip(frame.samples)
            {
                *output = sample as f32 / -(i16::MIN as f32);
            }
        } else {
            let counter = if self.started {
                &self.stats.missing_frames
            } else {
                &self.stats.startup_frames
            };
            counter.fetch_add(1, Ordering::Relaxed);
            self.output.fill(0.0);
        }
        if self.fade_remaining > 0 {
            self.fade_remaining -= 1;
            let mix = self.fade_remaining as f32 / self.fade_frames as f32;
            for (output, from) in self.output.iter_mut().zip(self.fade_from) {
                *output = from * mix + *output * (1.0 - mix);
            }
        }
    }
}

impl Iterator for LivePlayback {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.channel == 0 {
            self.render_frame();
        }
        let sample = self.output[self.channel];
        self.channel = (self.channel + 1) % self.format.channels();
        Some(sample)
    }
}

impl Source for LivePlayback {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.format.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.format.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_playback(format: Format, latency_ms: usize) -> (PlaybackInput, LivePlayback) {
        LivePlayback::new(format, latency_ms, 0, Arc::default())
    }

    fn advance(playback: &mut LivePlayback, frames: usize) -> Vec<f32> {
        let samples = frames * playback.format.channels();
        playback.take(samples).collect()
    }

    #[test]
    fn prefill_and_recovery_wait_for_the_target_and_count_events_not_frames() {
        let (mut input, mut playback) = new_playback(Format::new(1, 1000).unwrap(), 10);
        input.push(1, 1000, 9, &[16000; 9]);
        assert_eq!(advance(&mut playback, 20), vec![0.0; 20]);
        assert_eq!(playback.consumer.slots(), 9);
        assert_eq!(playback.stats.startup_frames.load(Ordering::Relaxed), 20);
        assert_eq!(playback.stats.missing_frames.load(Ordering::Relaxed), 0);
        input.push(1, 1000, 1, &[16000]);
        assert!(advance(&mut playback, 10).iter().all(|s| *s > 0.0));
        let tail = advance(&mut playback, 5);
        assert!(tail.windows(2).all(|s| s[0] >= s[1]));
        assert_eq!(tail[4], 0.0);
        input.push(1, 1000, 9, &[16000; 9]);
        assert_eq!(advance(&mut playback, 10), vec![0.0; 10]);
        assert_eq!(playback.stats.underrun_events.load(Ordering::Relaxed), 1);
        assert_eq!(playback.stats.missing_frames.load(Ordering::Relaxed), 15);
        input.push(1, 1000, 1, &[16000]);
        let resumed = advance(&mut playback, 10);
        assert!(resumed[0] > 0.0 && resumed[0] < resumed[4]);
        assert_eq!(playback.stats.underrun_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn overflow_is_bounded_aligned_and_crossfaded_at_the_actual_gap() {
        let (mut input, mut playback) = new_playback(Format::new(2, 1000).unwrap(), 10);
        let capacity = input.producer.slots();
        let positive: Vec<_> = (0..capacity + 5).flat_map(|_| [16000, -16000]).collect();
        input.push(2, 1000, capacity + 5, &positive);
        assert_eq!(playback.consumer.slots(), capacity);
        assert_eq!(playback.stats.dropped_frames.load(Ordering::Relaxed), 5);
        advance(&mut playback, capacity - 10);
        let negative: Vec<_> = (0..10).flat_map(|_| [-16000, 16000]).collect();
        input.push(2, 1000, 10, &negative);
        let before_gap = advance(&mut playback, 10);
        assert!(before_gap
            .chunks_exact(2)
            .all(|f| f[0] > 0.0 && f[1] == -f[0]));
        let after_gap = advance(&mut playback, 5);
        assert!(
            after_gap[0] > 0.0,
            "do not jump immediately to the opposite polarity"
        );
        assert!(after_gap[8] < 0.0);
        assert!(after_gap.chunks_exact(2).all(|f| f[1] == -f[0]));
    }

    #[test]
    fn diagnostics_use_the_received_rate_and_format_changes_are_rejected() {
        for (channels, rate) in [(1, 48000), (2, 96000)] {
            let (mut input, mut playback) = new_playback(Format::new(channels, rate).unwrap(), 10);
            let frames = rate as usize / 100;
            input.push(channels, rate, frames, &vec![1; frames * channels]);
            advance(&mut playback, 1);
            assert!(playback.stats.status().starts_with("buffered: 10.0 ms"));
            let queued = playback.consumer.slots();
            input.push(channels, 44100, 1, &vec![1; channels]);
            input.push(channels, rate, 2, &[1]);
            assert_eq!(playback.consumer.slots(), queued);
            assert_eq!(playback.stats.rejected_buffers.load(Ordering::Relaxed), 2);
        }
        assert!(Format::new(0, 44100).is_none());
        assert!(Format::new(33, 44100).is_none());
        assert!(Format::new(2, 0).is_none());
        assert!(Format::new(2, 192001).is_none());
    }

    #[test]
    fn concurrent_handoff_keeps_stereo_frames_intact_without_waiting_for_a_producer() {
        let (mut input, mut playback) = new_playback(Format::new(2, 1000).unwrap(), 10);
        let producer = std::thread::spawn(move || {
            for _ in 0..1000 {
                input.push(2, 1000, 2, &[12000, -12000, 12000, -12000]);
                std::thread::yield_now();
            }
        });
        for _ in 0..5000 {
            let left = playback.next().unwrap();
            let right = playback.next().unwrap();
            assert_eq!(left, -right);
        }
        producer.join().unwrap();
        for _ in 0..5000 {
            playback.next();
        }
        assert!(playback.waiting);
        assert!(playback.consumer.slots() < playback.target_frames);
        assert_eq!(
            playback.stats.buffered_frames.load(Ordering::Relaxed),
            playback.consumer.slots()
        );
    }

    #[test]
    fn starvation_retains_a_partial_refill_until_the_target_is_reached() {
        let (mut input, mut playback) = new_playback(Format::new(2, 1000).unwrap(), 10);
        input.push(2, 1000, 10, &[12000; 20]);
        advance(&mut playback, 15);
        assert!(playback.waiting);

        input.push(2, 1000, 8, &[12000; 16]);
        assert_eq!(advance(&mut playback, 100), vec![0.0; 200]);
        assert_eq!(playback.consumer.slots(), 8);
        assert_eq!(playback.stats.underrun_events.load(Ordering::Relaxed), 1);

        input.push(2, 1000, 2, &[12000; 4]);
        assert!(advance(&mut playback, 10)
            .iter()
            .all(|sample| *sample > 0.0));
        assert_eq!(playback.consumer.slots(), 0);
        assert!(!playback.waiting);
    }

    #[test]
    fn minimum_latency_holds_a_full_wire_chunk_even_with_a_partial_prefill() {
        for channels in [1, 2] {
            let (mut input, mut playback) = new_playback(Format::new(channels, 44100).unwrap(), 1);
            let partial = playback.target_frames - 1;
            input.push(channels, 44100, partial, &vec![0; partial * channels]);
            advance(&mut playback, 100);
            assert_eq!(playback.consumer.slots(), partial);

            let chunk_frames = AudioBuffer::MAX_AUDIO_BYTES / size_of::<i16>() / channels;
            let chunk = vec![12000; chunk_frames * channels];
            input.push(channels, 44100, chunk_frames, &chunk);
            assert_eq!(playback.consumer.slots(), partial + chunk_frames);
            advance(&mut playback, partial + chunk_frames);
            for _ in 0..5 {
                input.push(channels, 44100, chunk_frames, &chunk);
                advance(&mut playback, chunk_frames);
            }
            assert_eq!(playback.stats.dropped_frames.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn peak_meter_measures_input_even_when_loud_frames_are_dropped() {
        for free in [0, 1] {
            let (mut input, playback) = new_playback(Format::new(1, 44100).unwrap(), 1);
            let filled = input.producer.slots() - free;
            input.push(1, 44100, filled, &vec![0; filled]);
            assert!(playback.stats.status().ends_with("peak: 0.0%"));

            input.push(1, 44100, 2, &[0, i16::MIN]);
            assert!(playback.stats.status().ends_with("peak: 100.0%"));
            assert_eq!(
                playback.stats.dropped_frames.load(Ordering::Relaxed),
                (2 - free) as u64
            );
        }
    }

    #[test]
    fn recovery_finishes_fading_to_silence_even_if_the_refill_arrives_early() {
        let (mut input, mut playback) = new_playback(Format::new(1, 1000).unwrap(), 10);
        input.push(1, 1000, 10, &[16000; 10]);
        advance(&mut playback, 10);
        let first = advance(&mut playback, 1)[0];
        input.push(1, 1000, 10, &[16000; 10]);
        let tail = advance(&mut playback, 4);
        assert!(first > tail[0]);
        assert!(tail.windows(2).all(|s| s[0] >= s[1]));
        assert_eq!(tail[3], 0.0);
        assert_eq!(playback.consumer.slots(), 10);
        assert_eq!(playback.stats.underrun_events.load(Ordering::Relaxed), 1);
        let resumed = advance(&mut playback, 5);
        assert!(resumed[0] > 0.0 && resumed[0] < resumed[4]);
    }

    #[test]
    fn buffer_duration_tracks_admitted_frames_while_the_renderer_is_idle() {
        let (mut input, mut playback) = new_playback(Format::new(2, 48000).unwrap(), 10);
        input.push(2, 48000, 480, &[0; 960]);
        assert!(playback.stats.status().starts_with("buffered: 10.0 ms"));
        advance(&mut playback, 120);
        assert!(playback.stats.status().starts_with("buffered: 7.5 ms"));
        input.push(2, 48000, 120, &[0; 240]);
        assert!(playback.stats.status().starts_with("buffered: 10.0 ms"));
    }

    #[test]
    fn format_rejections_before_and_after_startup_share_one_counter() {
        let rejected = Arc::new(AtomicU64::new(0));
        assert!(Format::from_initial_buffer(2, 192001, 1, &[0; 2], &rejected).is_none());
        assert!(Format::from_initial_buffer(2, 44100, 2, &[0; 2], &rejected).is_none());
        assert!(Format::from_initial_buffer(2, 44100, 0, &[], &rejected).is_none());
        assert_eq!(rejected.load(Ordering::Relaxed), 3);

        let format = Format::from_initial_buffer(2, 44100, 1, &[0; 2], &rejected).unwrap();
        let (mut input, playback) = LivePlayback::new(format, 10, 0, rejected.clone());
        assert!(playback.stats.status().contains("rejected buffers: 3"));
        input.push(1, 44100, 1, &[0]);
        assert_eq!(rejected.load(Ordering::Relaxed), 4);
        assert!(playback.stats.status().contains("rejected buffers: 4"));
    }

    #[test]
    fn output_burst_budget_prevents_loss_with_512_frame_receive_and_render_blocks() {
        let format = Format::new(2, 44100).unwrap();
        let (mut input, mut playback) = LivePlayback::new(format, 1, 50, Arc::default());
        for _ in 0..20 {
            for frames in [125, 125, 125, 125, 12] {
                input.push(2, 44100, frames, &vec![12000; frames * 2]);
            }
            advance(&mut playback, 512);
        }
        assert!(playback.started);
        assert_eq!(playback.stats.underrun_events.load(Ordering::Relaxed), 0);
        assert_eq!(playback.stats.missing_frames.load(Ordering::Relaxed), 0);
        assert_eq!(playback.stats.dropped_frames.load(Ordering::Relaxed), 0);
        assert!(playback
            .stats
            .status()
            .contains("effective prefill: 50.0 ms"));
    }
}
