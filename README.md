# Ableton Link Rust Implementation

This is a Rust implementation of [Ableton Link](https://ableton.github.io/link), a technology that synchronizes musical beat, tempo, and phase across multiple applications running on one or more devices. Applications on devices connected to a local network discover each other automatically and form a musical session in which each participant can perform independently: anyone can start or stop while still staying in time. Anyone can change the tempo, the others will follow. Anyone can join or leave without disrupting the session.

This Rust crate provides an asynchronous, safe implementation of the Link protocol, leveraging Rust's memory safety guarantees and Tokio's async runtime for efficient network operations.

## Features

* **Full Link Protocol Support**: Implements the complete Ableton Link specification for tempo and timeline synchronization
* **Async/Await**: Built on Tokio for efficient asynchronous network operations
* **Memory Safe**: Leverages Rust's ownership system to prevent common networking and concurrency bugs
* **Cross-Platform**: Works on macOS, Linux, and Windows
* **Session Management**: Automatic peer discovery and session state synchronization
* **Start/Stop Sync**: Optional synchronization of play/stop states across devices
* **Real-time Safe**: Provides separate audio and application session state APIs

## License

This Rust implementation is licensed under the [GNU General Public License v3.0](LICENSE), consistent with the original Ableton Link project.

## Quick Start

Add this to your `Cargo.toml` :

```toml
[dependencies]
ableton-link-rs = "0.1.0"
```

### Basic Usage

```rust
use ableton_link_rs::link::BasicLink;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a new Link instance with 120 BPM
    let mut link = BasicLink::new(120.0).await;
    
    // Enable Link (starts network discovery)
    link.enable().await;
    
    // Capture current session state
    let mut session_state = link.capture_app_session_state();
    
    // Get current tempo
    println!("Current tempo: {} BPM", session_state.tempo());
    
    // Change tempo
    let current_time = link.clock().micros();
    session_state.set_tempo(140.0, current_time);
    
    // Commit changes back to Link
    link.commit_app_session_state(session_state).await;
    
    Ok(())
}
```

## Building and Running Examples

The project includes a Rust version of LinkHut, demonstrating basic Link functionality:

```bash
# Clone the repository
git clone https://github.com/anweiss/ableton-link-rs.git
cd ableton-link-rs

# Build the project
cargo build

# Run the RustHut example natively
cargo run --example rusthut
```

### Running RustHut in Docker

For containerized deployment, you can run the rusthut example in Docker:

```bash
# Build the Docker image
docker build -t rusthut-app .

# Run the container with interactive mode and host networking
docker run -it --network host rusthut-app
```

Host networking is recommended for optimal Ableton Link peer discovery across the network.

### RustHut Example

The `rusthut` example provides an interactive command-line interface similar to the original LinkHut:

* `a`: Enable/disable Link
* `space`: Start/stop playback
* `w`/`e`: Decrease/increase tempo
* `r`/`t`: Decrease/increase quantum
* `s`: Enable/disable start/stop sync
* `q`: Quit

## API Overview

### BasicLink

The main entry point for using Ableton Link:

```rust
// Create a new Link instance
let mut link = BasicLink::new(120.0).await;

// Enable/disable Link
link.enable().await;
link.disable().await;

// Check status
let is_enabled = link.is_enabled();
let peer_count = link.num_peers();

// Session state management
let session_state = link.capture_app_session_state();
link.commit_app_session_state(session_state).await;
```

### SessionState

Represents the current state of the Link session:

```rust
let mut state = link.capture_app_session_state();

// Tempo operations
let tempo = state.tempo();
state.set_tempo(140.0, current_time);

// Beat/time operations
let beat = state.beat_at_time(current_time, 4.0);
let time = state.time_at_beat(1.0, 4.0);
let phase = state.phase_at_time(current_time, 4.0);

// Start/stop operations
state.set_is_playing(true, current_time);
let is_playing = state.is_playing();
```

## Audio Integration

For real-time audio applications, use the audio-specific session state methods:

```rust
// In your audio callback
let session_state = link.capture_audio_session_state();

// Use session_state to sync your audio...

// If you need to modify state from audio thread
link.commit_audio_session_state(modified_state);
```

**Note**: The current implementation's audio session state methods are placeholders. For production audio applications, these would need to be implemented with lock-free, real-time safe mechanisms.

## Time and Clocks

The Link implementation uses `chrono::Duration` for precise time handling:

```rust
use chrono::Duration;

let clock = link.clock();
let current_time = clock.micros(); // Returns Duration since epoch in microseconds
```

The `Clock` abstraction provides platform-specific implementations for obtaining high-resolution system time, essential for accurate synchronization.

## Architecture

The Rust implementation is structured with the following key modules:

* **`link`**: Main Link API and session management
* **`discovery`**: Peer discovery and network messaging
* **`clock`**: Platform-specific time sources
* **`timeline`**: Beat/tempo/phase calculations
* **`controller`**: Session state management and coordination

## Build Requirements

| Platform | Minimum Required |
|----------|------------------|
| All      | Rust 1.70+       |
| macOS    | macOS 10.15+     |
| Linux    | glibc 2.28+      |
| Windows  | Windows 10+      |

## Dependencies

Key dependencies include:

* **tokio**: Async runtime for network operations
* **chrono**: Precise time handling
* **bincode**: Efficient serialization for network messages
* **socket2**: Low-level socket operations
* **tracing**: Structured logging

## Differences from C++ Implementation

* **Async/Await**: Uses Rust's async/await for all network operations
* **Memory Safety**: Leverages Rust's ownership system to prevent data races
* **Error Handling**: Uses Rust's `Result` type for explicit error handling
* **Serialization**: Uses bincode instead of custom serialization
* **Logging**: Uses the `tracing` crate for structured logging

## Testing

Run the test suite with:

```bash
cargo test
```

For integration testing with the original C++ Link implementation:

```bash
./test_interop.sh
```

## Contributing

Contributions are welcome! Please ensure that:

1. All tests pass: `cargo test`
2. Code is formatted: `cargo fmt`
3. No clippy warnings: `cargo clippy`
4. Documentation is updated for new features

## Documentation

For more information about Ableton Link concepts and theory:

* [Official Ableton Link Documentation](https://ableton.github.io/link)
* [Rust API Documentation](https://docs.rs/ableton-link-rs) (when published)

## Compatibility

This implementation aims for full compatibility with the official Ableton Link specification and should interoperate seamlessly with applications using the official C++ Link library.

## Status

This is an early-stage implementation. While the core Link protocol is implemented and functional, some areas may need additional work for production use:

* Audio thread real-time safety optimizations
* Platform-specific optimizations
* Additional testing and validation

## Support

For questions about this Rust implementation, please open an issue on GitHub. For general Ableton Link questions or licensing inquiries, contact <link-devs@ableton.com>.
