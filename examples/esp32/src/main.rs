//! Ableton Link ESP32 Example
//!
//! Rust equivalent of the C++ Ableton Link ESP32 example.
//! Connects to WiFi, creates a Link session at 120 BPM, and blinks
//! the on-board LED (GPIO2) on every beat while printing session info.

use ableton_link_rs::link::BasicLink;
use esp_idf_hal::gpio::*;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::*;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ── WiFi credentials ────────────────────────────────────────────────
// Change these to match your network, or set the environment variables
// WIFI_SSID / WIFI_PASS at compile time via `env!()` if you prefer.
const WIFI_SSID: &str = "your-ssid";
const WIFI_PASS: &str = "your-password";

// ── Link parameters ─────────────────────────────────────────────────
const DEFAULT_TEMPO: f64 = 120.0;
const QUANTUM: f64 = 4.0;

fn main() -> anyhow::Result<()> {
    // ── 1. ESP-IDF early init ───────────────────────────────────────
    // Patch libc functions required by the Rust runtime on ESP-IDF.
    esp_idf_svc::sys::link_patches();
    // Set up the ESP-IDF logger so `log::info!()` etc. appear on the
    // serial monitor.
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("=== Ableton Link ESP32 Example ===");

    // ── 2. Peripherals & system services ────────────────────────────
    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Configure GPIO2 (on-board LED on most ESP32 dev-kits) as output.
    let mut led = PinDriver::output(peripherals.pins.gpio2)?;
    led.set_low()?;

    // ── 3. WiFi ─────────────────────────────────────────────────────
    info!("Connecting to WiFi SSID: {}", WIFI_SSID);
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASS.try_into().unwrap(),
        ..Default::default()
    }))?;

    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    info!("WiFi connected — IP: {}", ip_info.ip);

    // ── 4. Create Ableton Link session ──────────────────────────────
    // BasicLink::new() is async, so we spin up a small Tokio runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let link = rt.block_on(async {
        let mut link = BasicLink::new(DEFAULT_TEMPO).await;
        link.enable().await;
        info!("Link enabled at {} BPM", DEFAULT_TEMPO);
        link
    });

    // Wrap in Arc<Mutex<>> so both the monitor thread and the main
    // loop can access it.
    let link = Arc::new(Mutex::new(link));

    // ── 5. Monitoring task ──────────────────────────────────────────
    // Spawns a thread that prints peer count, tempo and beat position
    // once per second.
    let link_monitor = Arc::clone(&link);
    thread::Builder::new()
        .name("link-monitor".into())
        .stack_size(4096)
        .spawn(move || loop {
            if let Ok(link) = link_monitor.lock() {
                let state = link.capture_app_session_state();
                let clock = link.clock();
                let time = clock.micros();
                let tempo = state.tempo();
                let beat = state.beat_at_time(time, QUANTUM);
                let phase = state.phase_at_time(time, QUANTUM);
                let peers = link.num_peers();

                info!(
                    "peers: {} | tempo: {:.1} BPM | beat: {:.2} | phase: {:.2}",
                    peers, tempo, beat, phase,
                );
            }
            thread::sleep(Duration::from_secs(1));
        })?;

    // ── 6. LED blink loop (main thread) ─────────────────────────────
    // Turns the LED on when the phase is near zero (< 0.1 of a beat),
    // giving a visual pulse on each downbeat — matching the behaviour
    // of the C++ ESP32 example.
    info!("Entering LED blink loop (GPIO2)");
    loop {
        if let Ok(link) = link.lock() {
            let state = link.capture_app_session_state();
            let time = link.clock().micros();
            let phase = state.phase_at_time(time, QUANTUM);

            if phase < 0.1 {
                led.set_high()?;
            } else {
                led.set_low()?;
            }
        }

        // ~10 ms tick keeps LED timing responsive without busy-spinning.
        thread::sleep(Duration::from_millis(10));
    }
}
