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
any pull request that changes `src/**` without changing `README.md`. When a change
genuinely carries no documentation obligation — an internal refactor, or a bug fix
with no API surface — say so in the pull request body and either apply the
`docs-not-needed` label or add `<!-- docs-not-needed -->` to the body. Agent-authored
pull requests must use the marker, since `safe-outputs` pins their labels.

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

## LinkAudio (`audio` feature)

The optional, **off-by-default** `audio` feature (`audio = ["std"]`) enables `src/link_audio/`, a Rust port of upstream's LinkAudio audio-streaming subsystem. It is a separate UDP protocol layered beside Link Classic, not a change to it.

When modifying this module:

1. **No `unsafe`.** `src/link_audio/mod.rs` carries `#![forbid(unsafe_code)]`. Upstream's raw-pointer ring buffer is replaced by a channel-based buffer pool (`queue.rs`) and RAII buffer handles lending `&mut [i16]` (`sink.rs`). Never remove the attribute to make a change fit.
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
1. The main library should NOT depend on `esp-idf-svc` or `esp-idf-hal` — those are example-only deps
2. ESP32 platform code uses `#[cfg(target_os = "espidf")]` gates
3. The ESP32 example cannot be cross-compiled in CI — only structure checks are run
4. Test ESP32 clock changes by verifying the `ClockTrait` contract on native targets
