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
        Arc, Mutex,
    },
    time::Duration,
};

use ableton_link_rs::link_audio::LinkAudio;

const SAMPLE_RATE: u32 = 44100;
const NUM_CHANNELS: usize = 2;
const NUM_FRAMES: usize = 256;
const QUANTUM: f64 = 4.0;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rust".to_string());

    let mut link = LinkAudio::new(120.0, name.clone()).await?;
    link.enable().await;
    link.enable_link_audio(true);

    println!("LinkAudio peer '{}' on {}", name, link.audio_endpoint());
    println!("publishing channel 'sine', waiting for peers...");

    let sink = link.add_sink("sine", NUM_FRAMES * NUM_CHANNELS);

    link.set_channels_changed_callback(|| println!("-- channels changed --"));

    // Subscribe to the first channel published by another peer.
    let subscribed = Arc::new(Mutex::new(None));
    let frames_received = Arc::new(AtomicU64::new(0));

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
            if let Some(channel) = link.channels().into_iter().find(|c| c.id != sink.id()) {
                println!(
                    "subscribing to '{}' from peer '{}'",
                    channel.name, channel.peer_name
                );
                let counter = frames_received.clone();
                *subscription = Some(link.add_source(channel.id, move |handle| {
                    counter.fetch_add(handle.info.num_frames as u64, Ordering::Relaxed);
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
        if received > 0 && received % (SAMPLE_RATE as u64) < NUM_FRAMES as u64 {
            println!(
                "peers: {} | channels: {} | received frames: {}",
                link.num_peers(),
                link.channels().len(),
                received
            );
        }
    }
}
