# Changelog

## [0.3.0](https://github.com/anweiss/ableton-link-rs/compare/v0.2.0...v0.3.0) (2026-07-30)


### ⚠ BREAKING CHANGES

* crate no longer re-exports bincode types. Bumping version to 0.3.0.

### Features

* add ESP32 example and platform support ([#28](https://github.com/anweiss/ableton-link-rs/issues/28)) ([3e50dbf](https://github.com/anweiss/ableton-link-rs/commit/3e50dbf0f2d3d82615459e5339d2b641aa9704ce))
* **ci:** add agentic workflows to track upstream Ableton Link ([#53](https://github.com/anweiss/ableton-link-rs/issues/53)) ([6a2d128](https://github.com/anweiss/ableton-link-rs/commit/6a2d128dc9b234148c578bafd6196679401f4480))


### Bug Fixes

* add conventional commit prefixes to dependabot config ([#29](https://github.com/anweiss/ableton-link-rs/issues/29)) ([61ac355](https://github.com/anweiss/ableton-link-rs/commit/61ac355b174d5bf3472bdee29e621887082c6024))
* add explicit permissions to CI and conventional-commits workflows ([#12](https://github.com/anweiss/ableton-link-rs/issues/12)) ([ed03e71](https://github.com/anweiss/ableton-link-rs/commit/ed03e713c67b94a7cc50009231f6fcaf3a54f2c8))
* **ci:** let the port workflow actually push the submodule bump ([#58](https://github.com/anweiss/ableton-link-rs/issues/58)) ([e22588b](https://github.com/anweiss/ableton-link-rs/commit/e22588be0484a2213c279a6c25f2edfccc492dd9))
* **ci:** make port PR titles pass the conventional commit check ([#60](https://github.com/anweiss/ableton-link-rs/issues/60)) ([7575f52](https://github.com/anweiss/ableton-link-rs/commit/7575f5294f3e7eeb9e47217abca78509b14b5ece))
* **ci:** make the triage coverage check mechanical instead of asserted ([#63](https://github.com/anweiss/ableton-link-rs/issues/63)) ([4c97a8f](https://github.com/anweiss/ableton-link-rs/commit/4c97a8fea0104c92aa06fb23f1d80d2af54ca94e))
* **ci:** retrigger CI/audit/convcommit on release-please PRs via workflow_dispatch ([#37](https://github.com/anweiss/ableton-link-rs/issues/37)) ([88fd478](https://github.com/anweiss/ableton-link-rs/commit/88fd478256df74ddea3e3dc13cbfd7d089e104ea))
* **ci:** revert retrigger to PAT-based close/reopen for bot PRs ([#38](https://github.com/anweiss/ableton-link-rs/issues/38)) ([ac55037](https://github.com/anweiss/ableton-link-rs/commit/ac5503759c04f3141487a69f8a37c6040dee9098))
* **ci:** stop the watermark retiring commits that were never ported ([#61](https://github.com/anweiss/ableton-link-rs/issues/61)) ([63eb49a](https://github.com/anweiss/ableton-link-rs/commit/63eb49af2702d31d9b18fe72c33b5c233bca0de9))
* **ci:** stop upstream drift script dying with SIGPIPE ([#55](https://github.com/anweiss/ableton-link-rs/issues/55)) ([fa7993a](https://github.com/anweiss/ableton-link-rs/commit/fa7993ad7a6e76502850b212d881e4c229355b55))
* correct dependabot package ecosystems ([#16](https://github.com/anweiss/ableton-link-rs/issues/16)) ([9112dc7](https://github.com/anweiss/ableton-link-rs/commit/9112dc7abd22ad29b49f21c2c82df4b58837b1b7))
* **discovery:** propagate socket bind errors instead of panicking ([#43](https://github.com/anweiss/ableton-link-rs/issues/43)) ([a20616d](https://github.com/anweiss/ableton-link-rs/commit/a20616dd79dff954f6c526e344ae783d948d6b5b)), closes [#42](https://github.com/anweiss/ableton-link-rs/issues/42)
* enable SO_REUSEPORT on multicast discovery sockets ([3655b93](https://github.com/anweiss/ableton-link-rs/commit/3655b935c6c26e9a5f8c61327aae122d97643b94))
* iterate map values to satisfy clippy::for_kv_map ([#52](https://github.com/anweiss/ableton-link-rs/issues/52)) ([2b39845](https://github.com/anweiss/ableton-link-rs/commit/2b39845be0c1483a9b7345d8515f0993d3b58210))
* join and send discovery on every usable IPv4 interface ([#51](https://github.com/anweiss/ableton-link-rs/issues/51)) ([abcf815](https://github.com/anweiss/ableton-link-rs/commit/abcf81591777c479b55b22a145b33280c52a8ddf))
* output NodeId bytes as hex with 0x prefix in Display ([#64](https://github.com/anweiss/ableton-link-rs/issues/64)) ([9d51950](https://github.com/anweiss/ableton-link-rs/commit/9d5195055b00ebe0a1d21a5fb791115ca30cf651))
* pin 3rd party actions to commit SHAs ([95106d0](https://github.com/anweiss/ableton-link-rs/commit/95106d08829fe4b0fcfadae299802404b9f239d1))
* remove custom CodeQL workflow (default setup already configured) ([34f3c45](https://github.com/anweiss/ableton-link-rs/commit/34f3c453172bb52ec0a5f9dc2671000ae907feef))
* resolve CI failures (clippy, security-audit, codeql) ([b5fdfdd](https://github.com/anweiss/ableton-link-rs/commit/b5fdfdd4c8eb84c468350e4af7096e291f23b8b0))
* suppress panic output in null-byte thread name test ([#15](https://github.com/anweiss/ableton-link-rs/issues/15)) ([3ea6152](https://github.com/anweiss/ableton-link-rs/commit/3ea61523bab012e6086a75ddf3ff196fa4e6837d))


### Miscellaneous Chores

* release 0.3.0 ([f82f72c](https://github.com/anweiss/ableton-link-rs/commit/f82f72c4ab9c29ab83213cffa6f89eeaff25dd00))


### Code Refactoring

* replace sunset bincode with internal encoding module ([#36](https://github.com/anweiss/ableton-link-rs/issues/36)) ([be70eef](https://github.com/anweiss/ableton-link-rs/commit/be70eef3664ba4ad9bff1f9a0c58457859cc243c))

## [0.2.0](https://github.com/anweiss/ableton-link-rs/compare/v0.1.2...v0.2.0) (2026-04-07)


### Features

* add no_std support with alloc ([8cb0d91](https://github.com/anweiss/ableton-link-rs/commit/8cb0d9109ae948d5d3e47a485158d864ecf8a8af))
* add no_std support with alloc ([#9](https://github.com/anweiss/ableton-link-rs/issues/9)) ([8cb0d91](https://github.com/anweiss/ableton-link-rs/commit/8cb0d9109ae948d5d3e47a485158d864ecf8a8af))


### Bug Fixes

* auto-retrigger CI for release-please PRs ([#11](https://github.com/anweiss/ableton-link-rs/issues/11)) ([10b42cd](https://github.com/anweiss/ableton-link-rs/commit/10b42cd3a3cb222636729d82fd8de70d8212e306))
