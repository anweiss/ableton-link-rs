use ableton_link_rs::link::{BasicLink, SessionState};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

struct State {
    running: Arc<AtomicBool>,
    link: BasicLink,
    quantum: f64,
    is_playing: bool, // Track local playing state like C++ AudioEngine
}

impl State {
    async fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            link: BasicLink::new(120.0).await,
            quantum: 4.0,
            is_playing: false,
        }
    }
}

fn print_help() {
    println!();
    println!(" < R U S T  H U T >");
    println!();
    println!("usage:");
    println!("  enable / disable Link: a");
    println!("  start / stop: space");
    println!("  decrease / increase tempo: w / e");
    println!("  decrease / increase quantum: r / t");
    println!("  enable / disable start stop sync: s");
    println!("  quit: q");
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
) {
    let enabled = if link_enabled { "yes" } else { "no" };
    let beats = session_state.beat_at_time(time, quantum);
    let phase = session_state.phase_at_time(time, quantum);
    let start_stop = if start_stop_sync { "yes" } else { "no" };
    let playing_status = if is_playing { "[playing]" } else { "[stopped]" };

    print!(
        "{:<7} | {:<9} | {:<7.0} | {:<3} {:<11} | {:<7.2} | {:<7.2} | ",
        enabled,
        num_peers,
        quantum,
        start_stop,
        playing_status,
        session_state.tempo(),
        beats
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
    use tokio::io::{stdin, AsyncReadExt};

    let mut stdin = stdin();
    let mut buffer = [0u8; 1];

    loop {
        // Read input - this will block until input is available
        match stdin.read_exact(&mut buffer).await {
            Ok(_) => {
                let input = buffer[0] as char;

                let mut state_guard = state.lock().await;
                let mut session_state = state_guard.link.capture_app_session_state();

                match input {
                    'q' | '\x03' => {
                        // 'q' or Ctrl+C (0x03)
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
                        session_state.set_tempo(new_tempo, session_state.time_for_is_playing());
                        state_guard
                            .link
                            .commit_app_session_state(session_state)
                            .await;
                    }
                    'e' => {
                        let new_tempo = (session_state.tempo() + 1.0).min(999.0); // Max tempo like C++
                        session_state.set_tempo(new_tempo, session_state.time_for_is_playing());
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
            }
            Err(_) => {
                // EOF or error, exit gracefully
                break;
            }
        }
    }
}

fn disable_buffered_input() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use termios::{Termios, ECHO, ICANON, TCSANOW};

        let fd = io::stdin().as_raw_fd();
        if let Ok(mut termios) = Termios::from_fd(fd) {
            termios.c_lflag &= !ICANON;
            termios.c_lflag &= !ECHO;
            let _ = termios::tcsetattr(fd, TCSANOW, &termios);
        }
    }
}

fn enable_buffered_input() {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        use termios::{Termios, ECHO, ICANON, TCSANOW};

        let fd = io::stdin().as_raw_fd();
        if let Ok(mut termios) = Termios::from_fd(fd) {
            termios.c_lflag |= ICANON;
            termios.c_lflag |= ECHO;
            let _ = termios::tcsetattr(fd, TCSANOW, &termios);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(tokio::sync::Mutex::new(State::new().await));

    print_help();
    print_state_header();

    disable_buffered_input();

    // Start input handler
    let input_task = {
        let state = state.clone();
        tokio::spawn(handle_input(state))
    };

    // Main display loop
    while state.lock().await.running.load(Ordering::Relaxed) {
        let (mut session_state, link_enabled, num_peers, quantum, start_stop_sync) = {
            let state_guard = state.lock().await;
            let session_state = state_guard.link.capture_app_session_state();
            let link_enabled = state_guard.link.is_enabled();
            let num_peers = state_guard.link.num_peers();
            let quantum = state_guard.quantum;
            let start_stop_sync = state_guard.link.is_start_stop_sync_enabled();

            (
                session_state,
                link_enabled,
                num_peers,
                quantum,
                start_stop_sync,
            )
        };

        // Critical: Check for playing state transitions like C++ AudioEngine
        let mut should_commit = false;
        {
            let mut state_guard = state.lock().await;
            let local_is_playing = state_guard.is_playing;
            let session_is_playing = session_state.is_playing();

            if !local_is_playing && session_is_playing {
                // Transition from not playing to playing - reset beat to 0
                session_state.request_beat_at_start_playing_time(0.0, quantum);
                state_guard.is_playing = true;
                should_commit = true;
            } else if local_is_playing && !session_is_playing {
                // Transition from playing to not playing
                state_guard.is_playing = false;
            }
        }

        // Commit any timeline modifications
        if should_commit {
            let state_guard = state.lock().await;
            state_guard
                .link
                .commit_app_session_state(session_state)
                .await;
        }

        // Get the final state for display
        let (time, session_state, is_playing) = {
            let state_guard = state.lock().await;
            let time = state_guard.link.clock().micros();
            let session_state = state_guard.link.capture_app_session_state();
            let is_playing = session_state.is_playing();

            (time, session_state, is_playing)
        };

        print_state(
            time,
            &session_state,
            link_enabled,
            num_peers,
            quantum,
            start_stop_sync,
            is_playing,
        );

        sleep(Duration::from_millis(10)).await;
    }

    // Cleanup
    enable_buffered_input();
    input_task.abort();

    Ok(())
}
