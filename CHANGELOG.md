# Changelog

## [1.0.0](https://github.com/anweiss/ableton-link-rs/compare/v0.3.0...v1.0.0) (2026-09-03)


### ⚠ BREAKING CHANGES

* `Encode::encode_to` now returns `Result<(), EncodeError>` instead of `()`, and `EncodeError` gains its first constructible variant, `StringTooLong`. Downstream `Encode` implementations must return `Ok(())` and propagate nested encodes with `?`; callers must handle the `Result`. `EncodeError` is now `#[non_exhaustive]` so future variants are additive.

### Features

* add PartialEq for SessionState and ApiState ([#89](https://github.com/anweiss/ableton-link-rs/issues/89)) ([c9595fb](https://github.com/anweiss/ableton-link-rs/commit/c9595fbf6cebae7f364795bed632739c127c94ca))
* add String and i16 support to wire encoding layer ([#92](https://github.com/anweiss/ableton-link-rs/issues/92)) ([7d444a3](https://github.com/anweiss/ableton-link-rs/commit/7d444a37d512698f9782070a7cace85fd7132063))
* **agentics:** let the watch workflow maintain the backlog issue body ([#79](https://github.com/anweiss/ableton-link-rs/issues/79)) ([c358937](https://github.com/anweiss/ableton-link-rs/commit/c35893797e7bf53d26f18fc4125abd2e15ef98cb))
* **ci:** fix Copilot review batches with Opus 5 instead of the cloud agent ([#94](https://github.com/anweiss/ableton-link-rs/issues/94)) ([39a46f2](https://github.com/anweiss/ableton-link-rs/commit/39a46f2d83977a918edfc29e73e94bdcb155a150))
* **ci:** gate port PR auto-merge behind a Copilot review loop ([#84](https://github.com/anweiss/ableton-link-rs/issues/84)) ([6f45585](https://github.com/anweiss/ableton-link-rs/commit/6f4558577359df1110342fc382feea9203c327f7))
* **ci:** handle Copilot review states now that auto-approval is on ([#142](https://github.com/anweiss/ableton-link-rs/issues/142)) ([b5d04c1](https://github.com/anweiss/ableton-link-rs/commit/b5d04c1037b2ac967386cbcae60d3b51e619f8db))
* **ci:** version the upstream porting backlog as a file with per-item issues ([#101](https://github.com/anweiss/ableton-link-rs/issues/101)) ([f1a5006](https://github.com/anweiss/ableton-link-rs/commit/f1a50063d264f0e5ec7b87c8d094da960af68110))
* count interface-set changes in Messenger (gatewaysChanged) ([#121](https://github.com/anweiss/ableton-link-rs/issues/121)) ([6c13778](https://github.com/anweiss/ableton-link-rs/commit/6c137781cedd8f0518f04e7bcc673e1c197fcc24))
* notify on peer audio-endpoint changes ([#129](https://github.com/anweiss/ableton-link-rs/issues/129)) ([3ff21d0](https://github.com/anweiss/ableton-link-rs/commit/3ff21d0285384ca9e2b92334346f75b2415d6ff5))
* **platform:** add ThreadPriority API (partial port; IO-thread wiring still outstanding) ([#137](https://github.com/anweiss/ableton-link-rs/issues/137)) ([15443b3](https://github.com/anweiss/ableton-link-rs/commit/15443b3a81717a3a9332a879e734d007effa5dbb))


### Bug Fixes

* add SessionId to ClientState for correct grace-period propagation ([#83](https://github.com/anweiss/ableton-link-rs/issues/83)) ([fd6fdd4](https://github.com/anweiss/ableton-link-rs/commit/fd6fdd41f316e1dd06e9ccc5aab07565cfbbbd86))
* **audio:** decode Ableton Live audio buffers and stabilize playback ([#72](https://github.com/anweiss/ableton-link-rs/issues/72)) ([10f0725](https://github.com/anweiss/ableton-link-rs/commit/10f0725da4d9530b372142ab2304cdd644b7b15a))
* catch message-encode errors in sendUdpMessage ([#124](https://github.com/anweiss/ableton-link-rs/issues/124)) ([ad928c1](https://github.com/anweiss/ableton-link-rs/commit/ad928c19f65d9955992f26149f18930e9bc16fa3))
* **ci:** address the Copilot review findings from [#101](https://github.com/anweiss/ableton-link-rs/issues/101) ([#114](https://github.com/anweiss/ableton-link-rs/issues/114)) ([4dc1b1a](https://github.com/anweiss/ableton-link-rs/commit/4dc1b1ae4a1d78049bbb78f2be52ace1d79586dc))
* **ci:** announce a review-loop handoff instead of editing a sticky comment ([#155](https://github.com/anweiss/ableton-link-rs/issues/155)) ([7aff7c4](https://github.com/anweiss/ableton-link-rs/commit/7aff7c40e3cb0368d6bf0534a04b4680dbdfb7bf))
* **ci:** close the three gaps that made the review loop need a human ([#123](https://github.com/anweiss/ableton-link-rs/issues/123)) ([e7166a1](https://github.com/anweiss/ableton-link-rs/commit/e7166a159d68228891d1df067409afac4d0f1429))
* **ci:** close three narrative-drift gaps the first unblocked port run exposed ([#148](https://github.com/anweiss/ableton-link-rs/issues/148)) ([d8764d1](https://github.com/anweiss/ableton-link-rs/commit/d8764d1f39e6d67c54b2de2cc37d51760ee73e5c))
* **ci:** key the blocker exemption on a marker and stop using gh pr list ([#151](https://github.com/anweiss/ableton-link-rs/issues/151)) ([9fdd985](https://github.com/anweiss/ableton-link-rs/commit/9fdd9850630781bab102328a9605d7b89cefe025))
* **ci:** let the merge close the port item's tracking issue ([#133](https://github.com/anweiss/ableton-link-rs/issues/133)) ([42afc66](https://github.com/anweiss/ableton-link-rs/commit/42afc665077af480161a79050492f005382c0cd8))
* **ci:** let the upstream workflows actually write the backlog file ([#118](https://github.com/anweiss/ableton-link-rs/issues/118)) ([9b88ccd](https://github.com/anweiss/ableton-link-rs/commit/9b88ccdd1e68b7e55bd160102322343f77e0d29d))
* **ci:** put the blocker marker in the comment template, not a distant rule ([#153](https://github.com/anweiss/ableton-link-rs/issues/153)) ([ebb4f07](https://github.com/anweiss/ableton-link-rs/commit/ebb4f07b0207b7220f80c98ea381485d04eed4b1))
* **ci:** recompile agentic workflow locks and stop Dependabot bumping them ([#100](https://github.com/anweiss/ableton-link-rs/issues/100)) ([cf10b36](https://github.com/anweiss/ableton-link-rs/commit/cf10b3660aba255ef7247f4f8f6b8ab29b88a2e5))
* **ci:** request Copilot review over GraphQL, not REST ([#88](https://github.com/anweiss/ableton-link-rs/issues/88)) ([156bedf](https://github.com/anweiss/ableton-link-rs/commit/156bedfa2eae0bda4e12e01ea707fc41b9a3a87a))
* **ci:** request Copilot review with the PAT, not GITHUB_TOKEN ([#90](https://github.com/anweiss/ableton-link-rs/issues/90)) ([ae890ab](https://github.com/anweiss/ableton-link-rs/commit/ae890ab57f01f73a797dd8d56f3ffcfabdc522b8))
* **ci:** require a green Copilot verdict to sign off, not merely zero inline comments ([#146](https://github.com/anweiss/ableton-link-rs/issues/146)) ([3f991e1](https://github.com/anweiss/ableton-link-rs/commit/3f991e1749f2e1ec5d1e7925040a5866722e31ff))
* **ci:** stop asking the maintainer to approve held workflow runs ([#97](https://github.com/anweiss/ableton-link-rs/issues/97)) ([591a2bc](https://github.com/anweiss/ableton-link-rs/commit/591a2bcc5ac151eec1204837e29823c0e643c5b2))
* **ci:** stop gating port PRs on a `port/` branch prefix ([#87](https://github.com/anweiss/ableton-link-rs/issues/87)) ([56a752b](https://github.com/anweiss/ableton-link-rs/commit/56a752beb5e956f37d2a02458d74af3160dff692))
* **ci:** stop one undecidable item head-of-line blocking the port backlog ([#145](https://github.com/anweiss/ableton-link-rs/issues/145)) ([9acb159](https://github.com/anweiss/ableton-link-rs/commit/9acb1593b715877aaa39a583d22e3a6a5c394d42))
* **ci:** stop release PRs from needing a manual workflow approval every time ([#80](https://github.com/anweiss/ableton-link-rs/issues/80)) ([55a7fbf](https://github.com/anweiss/ableton-link-rs/commit/55a7fbffd36ed65de8d821fa9653cd97cfbc5f5d))
* **ci:** stop the release PR needing a manual workflow approval ([#126](https://github.com/anweiss/ableton-link-rs/issues/126)) ([8e5db03](https://github.com/anweiss/ableton-link-rs/commit/8e5db03a4b9023e84083e0343f8be13361cedc30))
* **ci:** stop the review loop merging past a threat-detection flag ([#131](https://github.com/anweiss/ableton-link-rs/issues/131)) ([e2388e8](https://github.com/anweiss/ableton-link-rs/commit/e2388e89f29071d4cc18b6028abc55387443088d))
* **ci:** treat `unstable status` as already-mergeable when queueing auto-merge ([#91](https://github.com/anweiss/ableton-link-rs/issues/91)) ([f77350d](https://github.com/anweiss/ableton-link-rs/commit/f77350daed2d9dd827774f985158218047e2c734))
* **ci:** unblock port PR runs promptly instead of waiting on a 30-minute sweep ([#93](https://github.com/anweiss/ableton-link-rs/issues/93)) ([61ac540](https://github.com/anweiss/ableton-link-rs/commit/61ac5400355b031564e37b401213781f3db72bc4))
* **ci:** unstall the review loop — writable target files, and a real Octokit method ([#128](https://github.com/anweiss/ableton-link-rs/issues/128)) ([0e06f65](https://github.com/anweiss/ableton-link-rs/commit/0e06f6534c698065f25b06916d406ac584d34f77))
* **ci:** verify reconciled backlog issues by number, not by a lagged list ([#112](https://github.com/anweiss/ableton-link-rs/issues/112)) ([822662b](https://github.com/anweiss/ableton-link-rs/commit/822662b1b7e0dceac938ffebd1c8f7f15b8fc8d8))
* deliver only joined multicast groups to the discovery sockets on Linux ([#152](https://github.com/anweiss/ableton-link-rs/issues/152)) ([ed0e4ec](https://github.com/anweiss/ableton-link-rs/commit/ed0e4ecfba414624e476f8c4ce6f7dd4aced89c3))
* enforce safe-by-default with deny(unsafe_code) instead of asking ([#141](https://github.com/anweiss/ableton-link-rs/issues/141)) ([6ff330b](https://github.com/anweiss/ableton-link-rs/commit/6ff330b9fc996c5d4173509fb19a69e19d5538df))
* explicitly skip aep6 payload entries for IPv6 audio endpoints ([#86](https://github.com/anweiss/ableton-link-rs/issues/86)) ([4f52e8f](https://github.com/anweiss/ableton-link-rs/commit/4f52e8fc1a67795cccb687cfa89a124f0e98c3f8))
* gate Controller dispatch loops on disable to stop callbacks after shutdown ([#147](https://github.com/anweiss/ableton-link-rs/issues/147)) ([78dc501](https://github.com/anweiss/ableton-link-rs/commit/78dc5017ae6d14cb7873d78458da6efecda414dd))
* return an error instead of panicking on oversized ping/pong messages ([#144](https://github.com/anweiss/ableton-link-rs/issues/144)) ([88ad7ce](https://github.com/anweiss/ableton-link-rs/commit/88ad7ce3c424468de6ef6c5937f8425a617c441c))
* scope the stranded-port detector to this workflow's own output ([#75](https://github.com/anweiss/ableton-link-rs/issues/75)) ([53f97b9](https://github.com/anweiss/ableton-link-rs/commit/53f97b911a26fb74334780c43e308cd564925ea1))
* tear down LinkAudio before Link on drop (SessionController shutdown ordering) ([#119](https://github.com/anweiss/ableton-link-rs/issues/119)) ([8446299](https://github.com/anweiss/ableton-link-rs/commit/8446299760bd165bd00bfb5737a3df54d841e589))

## [0.3.0](https://github.com/anweiss/ableton-link-rs/compare/v0.2.0...v0.3.0) (2026-08-10)


### ⚠ BREAKING CHANGES

* crate no longer re-exports bincode types. Bumping version to 0.3.0.

### Features

* add ESP32 example and platform support ([#28](https://github.com/anweiss/ableton-link-rs/issues/28)) ([3e50dbf](https://github.com/anweiss/ableton-link-rs/commit/3e50dbf0f2d3d82615459e5339d2b641aa9704ce))
* **audio:** port the upstream LinkAudio subsystem behind an optional `audio` feature ([#70](https://github.com/anweiss/ableton-link-rs/issues/70)) ([9354d0e](https://github.com/anweiss/ableton-link-rs/commit/9354d0eb5e7e783bf61794ff2e5f113648ac3b38))
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
* handle UDP send failures when announcing peer state ([#69](https://github.com/anweiss/ableton-link-rs/issues/69)) ([8af0c42](https://github.com/anweiss/ableton-link-rs/commit/8af0c4280cac62065ca0003f4ee3443351e9aa26))
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
