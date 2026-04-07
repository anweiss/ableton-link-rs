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
3. `ENCODING_CONFIG` lives in the crate root (`src/lib.rs`), not in `discovery/mod.rs`.
4. `PayloadEntryHeader` lives in `src/link/encoding.rs` (shared no_std module), not in `payload.rs`.
5. Always verify both compilation modes:
   - `cargo check --lib --no-default-features` (no_std)
   - `cargo clippy --all-targets --all-features -- -D warnings` (std)

## CI and Branch Protection

The `main` branch requires all of the following status checks to pass before merging:

- Format check
- Clippy (ubuntu, macOS, windows)
- Build (ubuntu, macOS, windows)
- Test (ubuntu, macOS, windows)
- no_std check
- Validate PR title

Tests must be run with `--test-threads=1` because many tests bind the Link multicast port (20808).

## Release Process

Releases are managed by [release-please](https://github.com/googleapis/release-please). Merging conventional commits to `main` automatically creates/updates a release PR. Merging the release PR creates a GitHub release and triggers the publish workflow.

## Testing

- Run tests serially: `cargo test --all -- --nocapture --test-threads=1`
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
