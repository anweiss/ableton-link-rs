use ableton_link_rs::link::{BasicLink, SessionState};
use cpal::traits::{DeviceTrait, HostTrait};
use rodio::{cpal, OutputStreamBuilder, Sink};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Channel-based audio metronome that's Send-safe
#[derive(Debug)]
enum MetronomeMessage {
    PlayClick { is_downbeat: bool },
    Stop,
}

struct MetronomeHandle {
    sender: mpsc::UnboundedSender<MetronomeMessage>,
    last_beat_time: Arc<std::sync::Mutex<Option<chrono::Duration>>>,
}

impl MetronomeHandle {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn background thread for audio handling (not tokio task to avoid Send issues)
        std::thread::spawn(move || {
            Self::audio_thread(rx);
        });

        println!("Audio metronome initialized successfully");
        Ok(Self {
            sender: tx,
            last_beat_time: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn audio_thread(mut rx: mpsc::UnboundedReceiver<MetronomeMessage>) {
        // Create audio stream in the background thread
        let stream = match OutputStreamBuilder::open_default_stream() {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("Failed to create audio stream: {}", e);
                return;
            }
        };
        let sink = Sink::connect_new(stream.mixer());

        // Use blocking receive since we're in a regular thread
        while let Some(msg) = rx.blocking_recv() {
            match msg {
                MetronomeMessage::PlayClick { is_downbeat } => {
                    if let Err(e) = Self::play_click_sync(&sink, is_downbeat) {
                        eprintln!("Error playing click: {}", e);
                    }
                }
                MetronomeMessage::Stop => break,
            }
        }
    }

    fn play_click_sync(sink: &Sink, is_downbeat: bool) -> Result<(), Box<dyn std::error::Error>> {
        // Generate click audio exactly like C++ LinkHut
        let frequency = if is_downbeat { 1567.98 } else { 1108.73 }; // Same frequencies as C++
        let duration_ms = 100; // 100ms click duration like C++ version
        let sample_rate = 44100u32;
        let amplitude = 0.3f32;

        // Generate cosine wave with exponential decay envelope (matching C++ algorithm)
        let samples: Vec<f32> = (0..sample_rate * duration_ms / 1000)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                // Envelope: 1 - sin(5 * PI * t) for exponential decay
                let envelope = 1.0 - (5.0 * std::f32::consts::PI * t).sin();
                // Cosine wave: cos(2 * PI * freq * t)
                let wave = (2.0 * std::f32::consts::PI * frequency as f32 * t).cos();
                amplitude * wave * envelope
            })
            .collect();

        // Create audio source from samples
        let source = rodio::buffer::SamplesBuffer::new(1, sample_rate, samples);

        // Play the click sound
        sink.append(source);

        Ok(())
    }

    fn play_click(&self, is_downbeat: bool) -> Result<(), Box<dyn std::error::Error>> {
        if let Err(e) = self
            .sender
            .send(MetronomeMessage::PlayClick { is_downbeat })
        {
            eprintln!("Failed to send metronome message: {}", e);
        }
        Ok(())
    }

    fn check_and_play_beat(
        &self,
        session_state: &SessionState,
        current_time: chrono::Duration,
        quantum: f64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !session_state.is_playing() {
            return Ok(());
        }

        let beat_at_time = session_state.beat_at_time(current_time, quantum);

        // Only play for positive beats (no count-in), matching C++ behavior
        if beat_at_time < 0.0 {
            return Ok(());
        }

        let mut last_beat_time = self.last_beat_time.lock().unwrap();

        // Check if we've crossed a beat boundary using phase comparison (like C++ AudioEngine)
        if let Some(last_time) = *last_beat_time {
            // Check if the phase wraps around between the last time and current time
            let current_phase = session_state.phase_at_time(current_time, 1.0);
            let last_phase = session_state.phase_at_time(last_time, 1.0);

            // Phase wrap-around indicates a beat boundary crossing
            if current_phase < last_phase {
                // Calculate the precise time when the beat crossing occurred
                let beats_at_current = session_state.beat_at_time(current_time, quantum);
                let beat_number = beats_at_current.floor();
                let precise_beat_time = session_state.time_at_beat(beat_number, quantum);

                // Determine if this is a downbeat (quantum boundary)
                let phase_at_beat = session_state.phase_at_time(precise_beat_time, quantum);
                let is_downbeat = phase_at_beat.floor() as i64 % quantum as i64 == 0;

                self.play_click(is_downbeat)?;
            }
        }

        *last_beat_time = Some(current_time);
        Ok(())
    }
}

struct State {
    running: Arc<AtomicBool>,
    link: BasicLink,
    quantum: f64,
    is_playing: bool, // Track local playing state like C++ AudioEngine
    metronome: Option<MetronomeHandle>, // Optional in case audio initialization fails
    last_tempo_change: Option<(f64, std::time::Instant)>, // Track recent tempo changes for immediate display
}

impl State {
    async fn new() -> Self {
        // Try to create audio metronome, fall back to no audio if it fails
        let metronome = match MetronomeHandle::new() {
            Ok(m) => Some(m),
            Err(e) => {
                println!("Warning: Could not initialize audio metronome: {}", e);
                println!("Continuing without audio (visual metronome only)...");
                None
            }
        };

        Self {
            running: Arc::new(AtomicBool::new(true)),
            link: BasicLink::new(120.0).await,
            quantum: 4.0,
            is_playing: false,
            metronome,
            last_tempo_change: None,
        }
    }
}

fn print_help() {
    println!();
    println!(" < R U S T  H U T >");
    println!();
    println!("usage:");
    println!("  enable / disable Link: a");
    println!("  start / stop: space (plays audio metronome when enabled)");
    println!("  decrease / increase tempo: w / e");
    println!("  decrease / increase quantum: r / t");
    println!("  enable / disable start stop sync: s");
    println!("  quit: q or Ctrl+C");
    println!();
}

fn print_state_header() {
    println!("enabled | num peers | quantum | start stop sync | tempo   | beats   | metro");
}

fn clear_line() {
    print!("   \r");
    io::stdout().flush().unwrap();
}

fn print_state(
    time: chrono::Duration,
    session_state: &SessionState,
    link_enabled: bool,
    num_peers: usize,
    quantum: f64,
    start_stop_sync: bool,
    is_playing: bool,
    tempo_override: Option<f64>,
) {
    let enabled = if link_enabled { "yes" } else { "no" };
    let beats = session_state.beat_at_time(time, quantum);
    let phase = session_state.phase_at_time(time, quantum);
    let start_stop = if start_stop_sync { "yes" } else { "no" };
    let playing_status = if is_playing { "[playing]" } else { "[stopped]" };

    let tempo_to_display = tempo_override.unwrap_or_else(|| session_state.tempo());

    print!(
        "{:<7} | {:<9} | {:<7.0} | {:<3} {:<11} | {:<7.2} | {:<7.2} | ",
        enabled, num_peers, quantum, start_stop, playing_status, tempo_to_display, beats
    );

    for i in 0..(quantum.ceil() as i32) {
        if (i as f64) < phase {
            print!("X");
        } else {
            print!("O");
        }
    }
    clear_line();
}

async fn handle_input(state: Arc<tokio::sync::Mutex<State>>) {
    use std::io::Read;

    loop {
        // Check if we should exit before attempting to read
        if !state.lock().await.running.load(Ordering::Relaxed) {
            return;
        }

        // Use blocking read in a separate thread to avoid async issues
        let input_result = tokio::task::spawn_blocking(|| {
            let mut stdin = std::io::stdin();
            let mut buffer = [0u8; 1];
            match stdin.read_exact(&mut buffer) {
                Ok(_) => Some(buffer[0] as char),
                Err(_) => None,
            }
        })
        .await;

        if let Ok(Some(input)) = input_result {
            // Handle Ctrl+C as both character and signal
            if input as u8 == 0x03 {
                // Ctrl+C character (ETX) - handle it directly
                let state_guard = state.lock().await;
                state_guard.running.store(false, Ordering::Relaxed);
                drop(state_guard);
                enable_buffered_input();
                clear_line();
                std::process::exit(0);
            }

            let mut state_guard = state.lock().await;

            // Check running flag again after getting input
            if !state_guard.running.load(Ordering::Relaxed) {
                return;
            }

            let mut session_state = state_guard.link.capture_app_session_state();

            match input {
                'q' => {
                    // 'q' for quit
                    state_guard.running.store(false, Ordering::Relaxed);
                    clear_line();
                    return;
                }
                'a' => {
                    let enabled = state_guard.link.is_enabled();
                    if enabled {
                        state_guard.link.disable().await;
                    } else {
                        state_guard.link.enable().await;
                    }
                }
                'w' => {
                    let new_tempo = (session_state.tempo() - 1.0).max(20.0); // Min tempo like C++
                    let current_time = state_guard.link.clock().micros();
                    session_state.set_tempo(new_tempo, current_time);

                    // Store the tempo change for immediate display feedback
                    state_guard.last_tempo_change = Some((new_tempo, std::time::Instant::now()));

                    // Commit the change to Link in background
                    state_guard
                        .link
                        .commit_app_session_state(session_state)
                        .await;
                }
                'e' => {
                    let new_tempo = (session_state.tempo() + 1.0).min(999.0); // Max tempo like C++
                    let current_time = state_guard.link.clock().micros();
                    session_state.set_tempo(new_tempo, current_time);

                    // Store the tempo change for immediate display feedback
                    state_guard.last_tempo_change = Some((new_tempo, std::time::Instant::now()));

                    // Commit the change to Link in background
                    state_guard
                        .link
                        .commit_app_session_state(session_state)
                        .await;
                }
                'r' => {
                    state_guard.quantum = (state_guard.quantum - 1.0).max(1.0);
                }
                't' => {
                    state_guard.quantum = (state_guard.quantum + 1.0).min(16.0);
                    // Reasonable max
                }
                's' => {
                    let current_sync = state_guard.link.is_start_stop_sync_enabled();
                    state_guard.link.enable_start_stop_sync(!current_sync);
                }
                ' ' => {
                    let was_playing = session_state.is_playing();
                    if was_playing {
                        let current_time = state_guard.link.clock().micros();
                        session_state.set_is_playing(false, current_time);
                    } else {
                        let current_time = state_guard.link.clock().micros();
                        session_state.set_is_playing(true, current_time);
                    }
                    state_guard
                        .link
                        .commit_app_session_state(session_state)
                        .await;
                }
                _ => {}
            }
        } else {
            // No input or error, yield to other tasks
            tokio::task::yield_now().await;
        }
    }
}

fn disable_buffered_input() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use termios::{Termios, ECHO, ICANON, ISIG, TCSANOW};

        let fd = io::stdin().as_raw_fd();
        if let Ok(mut termios) = Termios::from_fd(fd) {
            termios.c_lflag &= !ICANON; // Disable canonical mode (line buffering)
            termios.c_lflag &= !ECHO; // Disable echo
            termios.c_lflag &= !ISIG; // Disable ISIG so Ctrl+C is read as character
            let _ = termios::tcsetattr(fd, TCSANOW, &termios);
        }
    }
}

fn enable_buffered_input() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use termios::{Termios, ECHO, ICANON, ISIG, TCSANOW};

        let fd = io::stdin().as_raw_fd();
        if let Ok(mut termios) = Termios::from_fd(fd) {
            termios.c_lflag |= ICANON; // Re-enable canonical mode
            termios.c_lflag |= ECHO; // Re-enable echo
            termios.c_lflag |= ISIG; // Ensure ISIG remains enabled for signal handling
            let _ = termios::tcsetattr(fd, TCSANOW, &termios);
        }
    }
}

fn print_audio_device_info() {
    match get_audio_device_info() {
        Ok(info) => {
            println!("SAMPLE RATE: {}", info.sample_rate);
            println!("DEVICE NAME: {}", info.device_name);
            match info.buffer_size_range {
                Some((min, max)) => {
                    // Use a typical buffer size (512 is common)
                    let typical_buffer_size = 512u32.clamp(min, max);
                    let buffer_duration_ms =
                        (typical_buffer_size as f64 / info.sample_rate as f64) * 1000.0;
                    println!(
                        "BUFFER SIZE: {} samples, {:.2} ms.",
                        typical_buffer_size, buffer_duration_ms
                    );

                    // Estimate latency (similar to LinkHut's output device latency)
                    let latency_samples = typical_buffer_size / 8; // Rough estimate
                    let latency_ms = (latency_samples as f64 / info.sample_rate as f64) * 1000.0;
                    println!(
                        "OUTPUT DEVICE LATENCY: {} samples, {:.5} ms.",
                        latency_samples, latency_ms
                    );
                }
                None => {
                    println!("BUFFER SIZE: Unknown");
                    println!("OUTPUT DEVICE LATENCY: Unknown");
                }
            }
            println!(); // Empty line for spacing
        }
        Err(e) => {
            println!("Failed to get audio device info: {}", e);
            println!(); // Empty line for spacing
        }
    }
}

struct AudioDeviceInfo {
    device_name: String,
    sample_rate: u32,
    buffer_size_range: Option<(u32, u32)>,
}

fn get_audio_device_info() -> Result<AudioDeviceInfo, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("No default output device found")?;
    let device_name = device.name()?;
    let default_config = device.default_output_config()?;

    let buffer_size_range = match default_config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => Some((*min, *max)),
        cpal::SupportedBufferSize::Unknown => None,
    };

    Ok(AudioDeviceInfo {
        device_name,
        sample_rate: default_config.sample_rate().0,
        buffer_size_range,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(tokio::sync::Mutex::new(State::new().await));

    // Print audio device information like C++ LinkHut
    print_audio_device_info();

    print_help();
    print_state_header();

    disable_buffered_input();

    // Start input handler
    let input_task = {
        let state = state.clone();
        tokio::spawn(handle_input(state))
    };

    // Start high-frequency beat detection task
    let beat_task = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_micros(100)); // 10kHz for precise timing
            loop {
                interval.tick().await;

                if !state.lock().await.running.load(Ordering::Relaxed) {
                    break;
                }

                let time = {
                    let state_guard = state.lock().await;
                    state_guard.link.clock().micros()
                };
                let (session_state, quantum) = {
                    let state_guard = state.lock().await;
                    let session_state = state_guard.link.capture_app_session_state();
                    let quantum = state_guard.quantum;
                    (session_state, quantum)
                };

                if let Some(ref metronome) = state.lock().await.metronome {
                    let _ = metronome.check_and_play_beat(&session_state, time, quantum);
                }
            }
        })
    };

    // Main display loop
    while state.lock().await.running.load(Ordering::Relaxed) {
        // Capture all state in a single lock acquisition to ensure consistency
        let (
            time,
            session_state,
            link_enabled,
            num_peers,
            quantum,
            start_stop_sync,
            should_commit,
            tempo_override,
        ) = {
            let mut state_guard = state.lock().await;
            let time = state_guard.link.clock().micros();
            let session_state = state_guard.link.capture_app_session_state();
            let link_enabled = state_guard.link.is_enabled();
            let num_peers = state_guard.link.num_peers();
            let quantum = state_guard.quantum;
            let start_stop_sync = state_guard.link.is_start_stop_sync_enabled();

            // Check for cached tempo change for immediate display feedback
            let tempo_override =
                if let Some((cached_tempo, change_time)) = state_guard.last_tempo_change {
                    let elapsed = change_time.elapsed();
                    if elapsed < std::time::Duration::from_millis(200) {
                        // Use cached tempo for immediate feedback (longer timeout)
                        Some(cached_tempo)
                    } else {
                        // Clear the cached tempo change after timeout
                        state_guard.last_tempo_change = None;
                        None
                    }
                } else {
                    None
                };

            // Check for playing state transitions
            let local_is_playing = state_guard.is_playing;
            let session_is_playing = session_state.is_playing();
            let mut should_commit = false;

            if !local_is_playing && session_is_playing {
                // Transition from not playing to playing - reset beat to 0
                let mut modified_session_state = session_state;
                modified_session_state.request_beat_at_start_playing_time(0.0, quantum);
                state_guard.is_playing = true;
                should_commit = true;
                (
                    time,
                    modified_session_state,
                    link_enabled,
                    num_peers,
                    quantum,
                    start_stop_sync,
                    should_commit,
                    tempo_override,
                )
            } else if local_is_playing && !session_is_playing {
                // Transition from playing to not playing
                state_guard.is_playing = false;
                (
                    time,
                    session_state,
                    link_enabled,
                    num_peers,
                    quantum,
                    start_stop_sync,
                    should_commit,
                    tempo_override,
                )
            } else {
                (
                    time,
                    session_state,
                    link_enabled,
                    num_peers,
                    quantum,
                    start_stop_sync,
                    should_commit,
                    tempo_override,
                )
            }
        };

        // Commit any timeline modifications outside the lock
        if should_commit {
            let mut state_guard = state.lock().await;
            state_guard
                .link
                .commit_app_session_state(session_state)
                .await;
        }

        let is_playing = session_state.is_playing();

        print_state(
            time,
            &session_state,
            link_enabled,
            num_peers,
            quantum,
            start_stop_sync,
            is_playing,
            tempo_override,
        );

        // Use a longer sleep to reduce display update frequency and prevent flickering
        sleep(Duration::from_millis(10)).await;
    }

    // Cleanup
    enable_buffered_input();
    input_task.abort();
    beat_task.abort();

    Ok(())
}
