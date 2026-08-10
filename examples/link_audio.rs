//! LinkAudio demo: publishes a channel of generated audio and subscribes to
//! the first channel discovered from another peer.
//!
//! Run with the optional `audio` feature:
//!
//! ```sh
//! cargo run --features audio --example link_audio
//! ```

use std::{
    collections::VecDeque,
    f64::consts::TAU,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use ableton_link_rs::link_audio::LinkAudio;
use rodio::{ChannelCount, OutputStreamBuilder, Sample, SampleRate, Sink, Source};

const SAMPLE_RATE: u32 = 44100;
const NUM_CHANNELS: usize = 2;
const NUM_FRAMES: usize = 256;
const QUANTUM: f64 = 4.0;

/// Default jitter buffer depth, in milliseconds, overridable with
/// `LINK_AUDIO_LATENCY_MS`.
///
/// This example does no clock-drift compensation between the sender and the
/// local output device, so the buffer has to absorb raw network jitter. On a
/// normal LAN that jitter was measured at roughly 30 ms peak to peak, so much
/// below this the stream underruns audibly.
const DEFAULT_LATENCY_MS: usize = 50;

/// Shared jitter buffer between the LinkAudio source callback (producer) and
/// the rodio output stream (consumer).
#[derive(Default)]
struct PlaybackQueue {
    samples: VecDeque<i16>,
    underruns: u64,
    /// Largest sample magnitude seen since the last status line, used as a
    /// simple input level meter.
    peak: i32,
    /// Wire format of the subscribed channel, learned from the first buffer.
    format: Option<(ChannelCount, SampleRate)>,
}

/// A rodio [`Source`] that plays whatever the LinkAudio source callback has
/// delivered, substituting silence on underrun so the stream never ends.
///
/// Samples are pulled a whole frame at a time. Popping individual samples would
/// let an underrun (or a trim) that is not a multiple of the channel count shift
/// every later sample by one channel, permanently swapping left and right.
struct LivePlayback {
    queue: Arc<Mutex<PlaybackQueue>>,
    channels: ChannelCount,
    sample_rate: SampleRate,
    frame: VecDeque<Sample>,
}

impl LivePlayback {
    fn new(
        queue: Arc<Mutex<PlaybackQueue>>,
        channels: ChannelCount,
        sample_rate: SampleRate,
    ) -> Self {
        Self {
            queue,
            channels,
            sample_rate,
            frame: VecDeque::with_capacity(channels as usize),
        }
    }
}

impl Iterator for LivePlayback {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if let Some(sample) = self.frame.pop_front() {
            return Some(sample);
        }

        let channels = self.channels as usize;
        let mut queue = self.queue.lock().unwrap();

        if queue.samples.len() >= channels {
            for _ in 0..channels {
                let sample = queue.samples.pop_front().unwrap_or(0);
                self.frame.push_back(sample as f32 / -(i16::MIN as f32));
            }
        } else {
            queue.underruns += 1;
            for _ in 0..channels {
                self.frame.push_back(0.0);
            }
        }

        self.frame.pop_front()
    }
}

impl Source for LivePlayback {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Opens the default output device and streams `queue` to the speakers until
/// the process exits.
fn start_playback(
    queue: Arc<Mutex<PlaybackQueue>>,
    channels: ChannelCount,
    sample_rate: SampleRate,
) {
    std::thread::spawn(move || {
        let stream = match OutputStreamBuilder::open_default_stream() {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("could not open an audio output device: {e}");
                return;
            }
        };

        let sink = Sink::connect_new(stream.mixer());
        sink.append(LivePlayback::new(queue, channels, sample_rate));
        sink.sleep_until_end();
    });
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ableton_link_rs=warn".into()),
        )
        .init();

    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rust".to_string());

    // Optional second argument: a case-insensitive substring of the channel to
    // subscribe to. A peer such as Ableton Live publishes one channel per
    // track, most of which are silent, so "Main" is a far more useful default
    // than "whatever was discovered first".
    let wanted = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "main".to_string());
    let wanted = wanted.to_lowercase();

    // Real-time monitoring of the subscribed channel is on by default; set
    // LINK_AUDIO_PLAYBACK=0 to receive without opening an output device.
    let playback_enabled = std::env::var("LINK_AUDIO_PLAYBACK").as_deref() != Ok("0");

    let latency_ms = std::env::var("LINK_AUDIO_LATENCY_MS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LATENCY_MS)
        .max(1);

    let mut link = LinkAudio::new(120.0, name.clone()).await?;
    link.enable().await;
    link.enable_link_audio(true);

    println!("LinkAudio peer '{}' on {}", name, link.audio_endpoint());
    println!("publishing channel 'sine', waiting for peers...");
    if playback_enabled {
        println!("received audio will be played on the default output device");
    }

    let sink = link.add_sink("sine", NUM_FRAMES * NUM_CHANNELS);

    link.set_channels_changed_callback(|| println!("-- channels changed --"));

    // Subscribe to the first channel published by another peer.
    let subscribed = Arc::new(Mutex::new(None));
    let frames_received = Arc::new(AtomicU64::new(0));
    let queue = Arc::new(Mutex::new(PlaybackQueue::default()));
    let mut playback_started = false;
    let mut listed = false;
    let mut last_status = std::time::Instant::now();

    let mut phase = 0.0f64;
    let mut beats_at_buffer_begin;

    loop {
        tokio::time::sleep(Duration::from_millis(
            (NUM_FRAMES as u64 * 1000) / SAMPLE_RATE as u64,
        ))
        .await;

        // Discover and subscribe.
        let mut subscription = subscribed.lock().unwrap();
        if subscription.is_none() {
            let candidates: Vec<_> = link
                .channels()
                .into_iter()
                .filter(|c| c.id != sink.id())
                .collect();

            // Prefer a name match, but fall back to the first remote channel so
            // the example still does something useful against a simple peer.
            let chosen = candidates
                .iter()
                .find(|c| c.name.to_lowercase().contains(&wanted))
                .or_else(|| candidates.first());

            if let Some(channel) = chosen {
                if !listed {
                    listed = true;
                    println!("{} channel(s) available:", candidates.len());
                    for c in &candidates {
                        println!("  {} ({})", c.name, c.peer_name);
                    }
                }

                println!(
                    "subscribing to '{}' from peer '{}'",
                    channel.name, channel.peer_name
                );
                let counter = frames_received.clone();
                let playback = playback_enabled.then(|| queue.clone());
                *subscription = Some(link.add_source(channel.id, move |handle| {
                    counter.fetch_add(handle.info.num_frames as u64, Ordering::Relaxed);

                    if let Some(queue) = &playback {
                        let mut queue = queue.lock().unwrap();
                        queue.format.get_or_insert((
                            handle.info.num_channels.max(1) as ChannelCount,
                            handle.info.sample_rate,
                        ));
                        let max_frames = handle.info.sample_rate as usize * latency_ms / 1000;
                        let channels = handle.info.num_channels.max(1);
                        let capacity = (max_frames * channels).max(handle.samples.len());
                        let queued = queue.samples.len();
                        if queued + handle.samples.len() > capacity {
                            // Always drop whole frames so that the remaining
                            // samples stay aligned to channel boundaries.
                            let overflow = queued + handle.samples.len() - capacity;
                            let overflow = overflow.div_ceil(channels) * channels;
                            queue.samples.drain(..overflow.min(queued));
                        }
                        queue.samples.extend(handle.samples.iter().copied());

                        let peak = handle
                            .samples
                            .iter()
                            .map(|s| (*s as i32).abs())
                            .max()
                            .unwrap_or(0);
                        queue.peak = queue.peak.max(peak);
                    }
                }));
            }
        }
        drop(subscription);

        // Render and publish a sine tone aligned to the Link beat grid.
        let session_state = link.capture_app_session_state();
        let time = link.clock().micros();
        beats_at_buffer_begin = session_state.beat_at_time(time, QUANTUM);

        if let Some(mut buffer) = sink.buffer() {
            let tempo = session_state.tempo();
            let samples = buffer.samples_mut();
            let frequency = 440.0;
            for frame in 0..NUM_FRAMES {
                let value = (phase * TAU).sin();
                phase = (phase + frequency / SAMPLE_RATE as f64).fract();
                let sample = (value * i16::MAX as f64 * 0.2) as i16;
                for channel in 0..NUM_CHANNELS {
                    samples[frame * NUM_CHANNELS + channel] = sample;
                }
            }

            let committed = buffer.commit_with_session_state(
                &session_state,
                link.controller().session_id().0,
                beats_at_buffer_begin,
                QUANTUM,
                NUM_FRAMES,
                NUM_CHANNELS,
                SAMPLE_RATE,
            );

            if !committed {
                eprintln!("failed to commit buffer at {tempo} bpm");
            }
        }

        let received = frames_received.load(Ordering::Relaxed);

        // The wire format is discovered from the first buffer that arrives, so
        // the output device can only be opened once audio is actually flowing.
        if playback_enabled && !playback_started {
            if let Some((channels, rate)) = queue.lock().unwrap().format {
                println!("playing back {channels} channel(s) at {rate} Hz");
                start_playback(queue.clone(), channels, rate);
                playback_started = true;
            }
        }

        if last_status.elapsed() >= Duration::from_secs(2) {
            last_status = std::time::Instant::now();
            let (underruns, peak, queued) = {
                let mut queue = queue.lock().unwrap();
                let peak = std::mem::take(&mut queue.peak);
                (queue.underruns, peak, queue.samples.len())
            };
            println!(
                "peers: {} | tempo: {:.1} | channels: {} | frames: {} | buffered: {:.1} ms | underruns: {} | peak: {:.1}%",
                link.num_peers(),
                session_state.tempo(),
                link.channels().len(),
                received,
                queued as f32 * 1000.0 / (SAMPLE_RATE as f32 * NUM_CHANNELS as f32),
                underruns,
                peak as f32 * 100.0 / -(i16::MIN as f32)
            );
        }
    }
}
