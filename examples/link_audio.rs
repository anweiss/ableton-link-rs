//! LinkAudio demo: publishes a channel of generated audio and subscribes to
//! the first channel discovered from another peer.
//!
//! Run with the optional `audio` feature:
//!
//! ```sh
//! cargo run --features audio --example link_audio
//! ```

use std::{
    f64::consts::TAU,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use ableton_link_rs::link_audio::LinkAudio;
use rodio::{Player, Source};

#[path = "common/audio_sink.rs"]
mod audio_sink;
#[path = "common/link_audio_playback.rs"]
mod link_audio_playback;

use link_audio_playback::{Format, LivePlayback, PlaybackInput, PlaybackStats};

const SAMPLE_RATE: u32 = 44100;
const NUM_CHANNELS: usize = 2;
const NUM_FRAMES: usize = 256;
const QUANTUM: f64 = 4.0;

/// Startup and recovery prefill target, overridable with `LINK_AUDIO_LATENCY_MS`.
/// Storage also reserves full-wire-chunk headroom; clock drift is not compensated.
const DEFAULT_LATENCY_MS: usize = 50;
const DEFAULT_OUTPUT_BURST_MS: usize = 50;

/// Opens the default output device and keeps the playback source alive.
fn start_playback(playback: LivePlayback) {
    std::thread::spawn(move || {
        let stream = match audio_sink::open_default_sink() {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("could not open an audio output device: {e}");
                return;
            }
        };

        let sink = Player::connect_new(stream.mixer());
        sink.append(playback);
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
        .clamp(1, 1000);
    let output_burst_ms = std::env::var("LINK_AUDIO_OUTPUT_BURST_MS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_OUTPUT_BURST_MS)
        .clamp(1, 1000);

    let mut link = LinkAudio::new(120.0, name.clone()).await?;
    if std::env::var("LINK_IO_REALTIME").as_deref() == Ok("1") {
        if let Err(error) = link.set_io_thread_priority(true).await {
            eprintln!("Link IO priority request failed; continuing at ordinary priority: {error}");
        }
    }
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
    let mut subscribed = None;
    let frames_received = Arc::new(AtomicU64::new(0));
    let rejected_buffers = Arc::new(AtomicU64::new(0));
    let (playback_tx, playback_rx) = mpsc::sync_channel(1);
    let mut playback_stats: Option<Arc<PlaybackStats>> = None;
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
        if subscribed.is_none() {
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
                let playback_tx = playback_tx.clone();
                let rejected = rejected_buffers.clone();
                let mut input: Option<PlaybackInput> = None;
                let mut warned_format = false;
                subscribed = Some(link.add_source(channel.id, move |handle| {
                    counter.fetch_add(handle.info.num_frames as u64, Ordering::Relaxed);

                    if playback_enabled {
                        if input.is_none() {
                            let format = Format::from_initial_buffer(
                                handle.info.num_channels,
                                handle.info.sample_rate,
                                handle.info.num_frames,
                                handle.samples,
                                &rejected,
                            );
                            if let Some(format) = format {
                                let (writer, playback) = LivePlayback::new(
                                    format,
                                    latency_ms,
                                    output_burst_ms,
                                    rejected.clone(),
                                );
                                input = Some(writer);
                                let _ = playback_tx.try_send(playback);
                            } else if !warned_format {
                                eprintln!("skipping unsupported or malformed playback format");
                                warned_format = true;
                            }
                        }
                        if let Some(input) = &mut input {
                            input.push(
                                handle.info.num_channels,
                                handle.info.sample_rate,
                                handle.info.num_frames,
                                handle.samples,
                            );
                        }
                    }
                }));
            }
        }

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
        if let Ok(playback) = playback_rx.try_recv() {
            println!(
                "playing back {} channel(s) at {} Hz; requested prefill: {latency_ms} ms; effective prefill: {:.1} ms",
                playback.channels(),
                playback.sample_rate(),
                playback.stats.prefill_ms(),
            );
            playback_stats = Some(playback.stats.clone());
            start_playback(playback);
        }

        if last_status.elapsed() >= Duration::from_secs(2) {
            last_status = std::time::Instant::now();
            let status = playback_stats
                .as_ref()
                .map(|stats| stats.status())
                .unwrap_or_else(|| {
                    format!(
                        "playback inactive | rejected buffers: {}",
                        rejected_buffers.load(Ordering::Relaxed)
                    )
                });
            println!(
                "peers: {} | tempo: {:.1} | channels: {} | frames: {} | {status}",
                link.num_peers(),
                session_state.tempo(),
                link.channels().len(),
                received,
            );
        }
    }
}
