# Copilot Instructions for ableton-link-rs

## README Maintenance

Any time changes to the library warrant a documentation update, the `README.md` **must** be updated in the same PR. This includes but is not limited to:

- New features or feature flags
- API changes (new types, renamed methods, removed functionality)
- Changes to build requirements or supported platforms
- New or removed dependencies
- New examples or changed example behavior
- Changes to the minimum supported Rust version (MSRV)
- Changes to the project status or maturity

This is enforced by CI, not left to review. The **README maintenance** check fails
any pull request that changes `src/**`, `examples/**` or `Cargo.toml` without changing
`README.md`. `Cargo.toml` is guarded because feature flags, dependencies and the MSRV
live there, and `examples/**` because new or changed examples are on the list above.
Pull requests labelled `autorelease: pending` or `dependencies` are exempt — version
bumps and dependency bumps are machine-generated and carry no documentation
obligation. When a change
genuinely carries no documentation obligation — an internal refactor, or a bug fix
with no API surface — say so in the pull request body and either apply the
`docs-not-needed` label or add `<!-- docs-not-needed -->` to the body. The marker is
honoured only on `automation`-labelled pull requests, because a body marker can be
self-applied; agent-authored pull requests rely on it, since `safe-outputs` pins their
labels and they carry `automation` already.

When updating the README:

1. Update the version number in all code examples and dependency snippets
2. Update the feature list to reflect current capabilities
3. Update API examples if method signatures or behavior changed
4. Update the architecture section if module structure changed
5. Update build requirements if toolchain or platform requirements changed
6. Keep the `no_std` section accurate with current core vs std-only types

## Conventional Commits

All commits must follow the [Conventional Commits](https://www.conventionalcommits.org/) specification. PR titles are used as squash-merge commit messages and must also follow this format:

- `feat:` — new features (triggers minor version bump)
- `fix:` — bug fixes (triggers patch version bump)
- `docs:` — documentation changes
- `ci:` — CI/workflow changes
- `chore:` — maintenance tasks
- `refactor:` — code restructuring without behavior change
- `test:` — test additions or changes
- `perf:` — performance improvements

Append `!` after the type for breaking changes (e.g., `feat!:`) which triggers a major version bump.

## Feature Flags and `no_std`

This library supports `no_std` environments via the `std` feature flag (enabled by default). When modifying code:

1. **Core types** (`Beats`, `Tempo`, `Timeline`, `GhostXForm`, `StartStopState`, `NodeId`, phase math, median, linear regression, encoding) must remain `no_std`-compatible — use `core::` and `alloc::` instead of `std::`.
2. **Networking modules** (controller, sessions, measurement, pingresponder, payload, discovery, clock, `BasicLink`) are gated behind `#[cfg(feature = "std")]`.
3. Wire encoding lives in `src/encoding.rs` — a hand-rolled, `no_std`-compatible big-endian fixed-int layer exposing the `Encode`/`Decode` traits plus `encode_to_vec` / `decode_from_slice`. The crate no longer depends on `bincode`; do not reintroduce it or the old `ENCODING_CONFIG` constant.
4. `PayloadEntryHeader` lives in `src/link/encoding.rs` (shared no_std module), not in `payload.rs`.
5. Always verify all compilation modes:
   - `cargo check --lib --no-default-features` (no_std)
   - `cargo check --all-targets` (default features — `audio` off)
   - `cargo clippy --all-targets --all-features -- -D warnings` (std + audio)

## Safe Rust by Default

`Cargo.toml` sets `unsafe_code = "deny"` under `[lints.rust]`, so this rule is
enforced by the compiler on every CI target, not by review. It is set there
rather than as `#![deny(...)]` in `src/lib.rs` deliberately: `--all-targets`
builds `examples/` and `tests/` as separate crates, which a library inner
attribute does not reach.

This is a port of a C++ library, which means the tempting shape for every
platform-facing item is a hand-written FFI shim. That is almost never the right
answer here. **Before writing any `unsafe`, look for a crate that already wraps
the OS mechanism**, and prefer it even when it is not a byte-for-byte match —
a vetted crate is tested on every platform this crate ships to, which a
hand-rolled shim written against documentation is not.

Worked example: `platform::ThreadPriority` (`src/platform/thread.rs`) drives
`SCHED_FIFO` on Linux, the mach time-constraint policy on macOS and MMCSS on
Windows — upstream's three mechanisms — entirely through the
`audio_thread_priority` crate, with no `unsafe`. Where the crate's parameters
differ from upstream's, the deviations are documented on the type rather than
worked around with FFI.

If nothing safe will do:

1. Add `#[allow(unsafe_code)]` at the **narrowest** scope that works — a module
   or an item, never the crate root.
2. Put a comment next to it saying which crates you evaluated and why each was
   rejected.
3. Say the same thing in the pull request body. An `#[allow(unsafe_code)]` with
   no stated alternative is a review blocker.

One place does this today: `examples/rusthut.rs` (Windows console mode), which
names the crates it evaluated and why they were rejected.

Note what is *not* on that list. `src/platform/clock.rs` reads ESP-IDF's
`esp_timer_get_time` through `esp_idf_svc::timer::EspTaskTimerService::now()`,
a safe wrapper over the identical call. A target-gated dependency
(`[target.'cfg(target_os = "espidf")'.dependencies]`) does not reach hosted
targets, so "that crate drags in a build script" is not on its own a reason to
hand-roll FFI — verify the claim with
`cargo metadata --filter-platform <host-triple>` before relying on it.

## LinkAudio (`audio` feature)

The optional, **off-by-default** `audio` feature (`audio = ["std"]`) enables `src/link_audio/`, a Rust port of upstream's LinkAudio audio-streaming subsystem. It is a separate UDP protocol layered beside Link Classic, not a change to it.

When modifying this module:

1. **No `unsafe`.** On top of the crate-wide `deny` above, `src/link_audio/mod.rs` carries `#![forbid(unsafe_code)]`, which cannot be opted out of with `#[allow]`. Upstream's raw-pointer ring buffer is replaced by a channel-based buffer pool (`queue.rs`) and RAII buffer handles lending `&mut [i16]` (`sink.rs`). Never remove the attribute to make a change fit.
2. **Test with `--all-features`.** A bare `cargo test --all` compiles none of `src/link_audio/` and passes without running a single audio test.
3. **Keep the default build working.** Verify with `cargo check --all-targets` that an audio-only change has not broken the audio-less configuration most users get.
4. **Wire format.** `src/link_audio/{messages,payload,encoding,codec}.rs` put bytes on the network. LinkAudio length-prefixes strings and vectors with a **u32**, unlike Link Classic. The `aep4` peer-state entry (`src/link/audio_endpoint.rs`) is Link Classic wire format and is how audio peers discover each other.
5. The runnable demo is `examples/link_audio.rs` (`cargo run --example link_audio --features audio`).

## CI and Branch Protection

The `main` branch requires all of the following status checks to pass before merging:

- Format check
- Clippy (ubuntu, macOS, windows)
- Build (ubuntu, macOS, windows)
- Test (ubuntu, macOS, windows)
- no_std check
- Default feature build
- Validate PR title

Tests must be run with `--test-threads=1` because many tests bind the Link multicast port (20808).

## Release Process

Releases are managed by [release-please](https://github.com/googleapis/release-please). Merging conventional commits to `main` automatically creates/updates a release PR. Merging the release PR creates a GitHub release and triggers the publish workflow.

## Testing

- Run tests serially: `cargo test --all --all-features -- --nocapture --test-threads=1`
- `--all-features` is required to exercise the optional `audio` (LinkAudio) module
- Tests using real UDP multicast are marked `#[ignore]` — run with `--include-ignored` locally
- macOS CI runners lack multicast routing, so some network tests may fail there

## ESP32 Support

The library includes an ESP32 platform clock (`EspClock`) gated behind `#[cfg(target_os = "espidf")]`.
The `examples/esp32/` directory is a standalone Cargo project (not a workspace member) targeting
`xtensa-esp32-espidf` with its own toolchain and dependencies.

When modifying ESP32-related code:
1. The library takes `esp-idf-svc` only as a `cfg(target_os = "espidf")` target
   dependency, for the safe `EspTaskTimerService::now()` clock read. Keep it
   target-gated — it must never enter the host dependency graph. Verify with
   `cargo metadata --filter-platform $(rustc -vV | grep host | cut -d' ' -f2)`.
   `esp-idf-hal` and `esp-idf-sys` remain example-only.
2. ESP32 platform code uses `#[cfg(target_os = "espidf")]` gates
3. The ESP32 example cannot be cross-compiled in CI — only structure checks are
   run, so `cfg(target_os = "espidf")` code is **never compiled by CI**. Changes
   to it carry no automated proof; review them by hand against the upstream API.
4. Test ESP32 clock changes by verifying the `ClockTrait` contract on native targets
