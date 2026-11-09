# Building and running examples

[Documentation index](README.md) | [Project README](../README.md)

Repository paths and shell commands below are relative to the repository root.

```bash
# Clone the repository
git clone https://github.com/anweiss/ableton-link-rs.git
cd ableton-link-rs

# Build the project
cargo build

# Run the RustHut example
cargo run --example rusthut

# Run the LinkAudio example (requires the optional audio feature).
# The second argument selects which remote channel to subscribe to by
# (case-insensitive) substring; it defaults to "main".
cargo run --features audio --example link_audio -- my-peer-name main

# Run the platform optimizations demo
cargo run --example platform_demo
```

For LinkAudio playback, latency settings and receive-only mode, see
[LinkAudio integration and playback](api.md#linkaudio-feature--audio).
For platform dependencies, see [build requirements](development.md#build-requirements).

## Running RustHut in Docker

```bash
docker build -t rusthut-app .
docker run -it --network host rusthut-app
```

Host networking is required for Ableton Link peer discovery across the network.

## ESP32

The `examples/esp32/` directory contains a standalone ESP32 project that mirrors the
[C++ Link ESP32 example](https://github.com/Ableton/link/tree/master/examples/esp32).
It connects to WiFi, joins a Link session, and blinks the on-board LED on every beat.

**Prerequisites:** [espup](https://github.com/esp-rs/espup), ldproxy, espflash

```bash
cd examples/esp32
# Edit src/main.rs to set WIFI_SSID and WIFI_PASS
cargo build
espflash flash target/xtensa-esp32-espidf/debug/link-esp32-example --monitor
```

See [`examples/esp32/README.md`](../examples/esp32/README.md) for full setup instructions.

## RustHut Controls

| Key | Action |
|-----|--------|
| `a` | Enable/disable Link |
| `space` | Start/stop playback |
| `w` / `e` | Decrease / increase tempo |
| `r` / `t` | Decrease / increase quantum |
| `s` | Enable/disable start/stop sync |
| `q` | Quit |
