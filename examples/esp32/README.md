# Ableton Link — ESP32 Example

Rust port of the [C++ Ableton Link ESP32 example](https://github.com/Ableton/link/tree/master/examples/esp32).
Connects to WiFi, joins an Ableton Link session at 120 BPM, blinks the
on-board LED on every beat, and prints session info over the serial monitor.

## What it does

1. Initialises ESP-IDF (logging, NVS, networking, event loop)
2. Connects to a WiFi access point
3. Creates a `BasicLink` session at 120 BPM and enables Link discovery
4. Spawns a monitor thread that prints peer count, tempo and beat position
   once per second
5. Blinks GPIO2 (on-board LED on most dev-kits) when the Link phase is
   near the start of a bar (phase < 0.1)

## Prerequisites

| Tool | Install |
|------|---------|
| **espup** | `cargo install espup && espup install` |
| **ldproxy** | `cargo install ldproxy` |
| **espflash** | `cargo install espflash` |
| **Rust ESP toolchain** | Installed automatically by `espup install` |

> `espup install` downloads the Xtensa Rust toolchain and the ESP-IDF SDK.
> Make sure you source the generated export file (e.g. `. ~/export-esp.sh`)
> before building.

## Configure WiFi

Edit `src/main.rs` and set your network credentials:

```rust
const WIFI_SSID: &str = "your-ssid";
const WIFI_PASS: &str = "your-password";
```

## Build

```bash
cd examples/esp32
cargo build
```

The first build downloads and compiles ESP-IDF — this can take 10-20 minutes.
Subsequent builds are much faster.

## Flash & Monitor

```bash
espflash flash target/xtensa-esp32-espidf/debug/link-esp32-example --monitor
```

Or uncomment the `runner` line in `.cargo/config.toml` and use:

```bash
cargo run
```

## Expected behaviour

* **Serial output** (at 115200 baud):
  ```
  INFO  — Ableton Link ESP32 Example
  INFO  — WiFi connected — IP: 192.168.1.42
  INFO  — Link enabled at 120 BPM
  INFO  — peers: 0 | tempo: 120.0 BPM | beat: 12.34 | phase: 0.34
  INFO  — peers: 1 | tempo: 120.0 BPM | beat: 16.78 | phase: 0.78
  ```
* **LED** on GPIO2 flashes briefly at the start of every 4-beat bar.
* Launch Ableton Live (or any Link-compatible app) on the same network —
  the peer count should increase and tempos will synchronise.

## Troubleshooting

* **Build fails with "esp toolchain not found"** — run `espup install` and
  source the export script.
* **WiFi won't connect** — double-check SSID/password; ensure the ESP32 is
  in range of a 2.4 GHz network.
* **No peers detected** — make sure the ESP32 and the other Link peers are
  on the same subnet and that multicast traffic is not blocked.
