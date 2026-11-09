# Changelog

## [0.4.0](https://github.com/anweiss/ableton-link-rs/compare/v0.3.0...v0.4.0) (2026-11-09)


### ⚠ BREAKING CHANGES

* link::error::Error adds Protocol(&'static str). Update exhaustive matches and do not treat malformed headers or known entry size mismatches as DecodeError::UnexpectedEnd.
* AtomicSessionState::process_pending_updates and SafeRtSessionStateHandler::process_pending_updates are removed. Migrate to pending_update returning (ticket, proposal), validate before accepted publication, complete_pending_update, then take_commit_result before reusing the queue slot.
* BasicLink::capture_audio_session_state now returns Result<SessionState, AudioCaptureError>. AtomicSessionState audio capture and SafeRtSessionStateHandler::get_rt_client_state return Result<ClientState, AudioCaptureError>. Skip capture or reuse a caller-owned accepted value on Busy or ReaderExhausted; never retry inside the audio callback. App capture remains infallible but its accepted-cache fallback may block and must not be used as an audio-callback fallback.
* Remove the obsolete implicit triple_buffer feature from downstream feature lists. Managed timing publication can return TimelineOriginExhausted for a final session origin outside wire microseconds.
* The obsolete implicit triple_buffer Cargo feature is removed; downstream feature lists must stop requesting it. Managed timing publication returns TimelineOriginExhausted for a session origin that would saturate on the wire, including a combined timing/playback edit.
* update_client_timeline_from_session returns Result<Timeline, CommitError>, and controller::update_session_timing returns Result<(), CommitError>. Handle TimelineOriginExhausted for an unrepresentable canonical phase boundary, a zero/nonfinite inverse slope or an unstorable final origin. update_session_timeline_from_client also rejects an out-of-Chrono final origin instead of publishing its clamp. Managed rejection retains accepted state and capture provenance; best-effort lock-contention behavior of the lower-level timing helper is unchanged. PeerStateChange::SessionStartStopState now carries (SessionId, Timeline, StartStopState); retain the advertised timeline rather than substituting current timing.
* GhostXForm::ghost_to_host may return Chrono durations outside signed-i64 microseconds rather than clipping them to that wire range. Handle num_microseconds() returning None. Its single half-away-from-zero microsecond rounding and the signed-microsecond wire format are unchanged.
* AtomicSessionState::new and SafeRtSessionStateHandler::new take only the initial state. Capture/get methods no longer take time, and low-level commit/update methods take only the incoming proposal. Remove grace_period, set_grace_period, LOCAL_MOD_GRACE_PERIOD and the two RtClientState grace timestamps. BasicLink/LinkAudio drains take &self. Playback delivery coalesces intermediate undelivered values. Runtime NaN tempo becomes 120 BPM and other accepted runtime tempos are bounded to 20..999 BPM.
* AtomicSessionState::new and SafeRtSessionStateHandler::new take only the initial state. Capture/get methods no longer take time, and low-level commit/update methods take only the incoming proposal. Remove grace_period, set_grace_period, LOCAL_MOD_GRACE_PERIOD and the two RtClientState grace timestamps. BasicLink/LinkAudio drains take &self. Playback delivery coalesces intermediate undelivered values. Runtime NaN tempo becomes 120 BPM and other accepted runtime tempos are bounded to 20..999 BPM.
* The parent migration introduces fallible controller/app commits, checked timeline conversion, Clone-only captured SessionState values and explicitly acknowledged audio submissions. Handle exhaustion/stale errors, retain deferred capture provenance, and pump the bounded audio drain on a non-RT thread. Hardening-specific direct clock payload encoders retain their fallible signed-microsecond validation.
* update_session_timeline_from_client returns Result<Timeline, CommitError>; controller and app commits return Result<(), CommitError>. Handle exhausted counters and StaleClientState without assuming partial acceptance or automatic synchronization recovery. SessionState is Clone, not Copy; submit captures from the same BasicLink. Deferred controller callers use capture_client_state and set_state_checked with the original origin. Audio commits return Result<AudioCommitTicket, AudioQueueError>; pump process_audio_session_state_updates on a non-RT thread and acknowledge take_audio_commit_result. Low-level RT handlers use explicit proposal, accepted publication, completion and acknowledgment; process_pending_updates only peeks.
* Chrono 0.4.39 is now the minimum supported version. Completed timeline and forward-clock conversions may return Chrono durations wider than signed microseconds; callers of num_microseconds() must handle None. Existing RemeasurementTask ownership, Option-owner handler, Sessions::new, incarnation-key reset_timeline and owner-typed update_discovery migrations remain required.
* minimum Chrono is now 0.4.39. Derived tempo, timeline and forward-clock durations may exceed signed microseconds; callers must handle num_microseconds returning None. Timestamp encoders saturate at the final wire boundary. Prior RemeasurementTask ownership and coordinator migrations remain required.
* controller::update_discovery now takes &Controller instead of separate session_state, peer_state and discovery arguments. The owner supplies a coherent current identity, timing and responder publication boundary.
* controller::update_discovery now takes &Controller instead of separate session_state, peer_state and discovery arguments. The owner supplies a coherent current identity, timing and responder publication boundary.
* Retry helpers return abort-on-drop RemeasurementTask owners; Cancelled includes the attempted NodeId. Construct Sessions through new and pass captured origin and mapping to reset_timeline. The full source migration is documented in README.md. Final hosted validation and fresh review are still required before merging; #154 remains open.
* Cancelled includes the attempted NodeId. Retry helpers return abort-on-drop RemeasurementTask owners. Construct Sessions through new; reset_timeline requires the captured SessionKey and clock mapping and returns bool. Disable resets the local incarnation; beat arithmetic saturates.
* schedule_remeasurement returns JoinHandle<()>; retain and abort independent scheduling ownership when it ends. Each task waits for a peer and sends at most one request.
* schedule_remeasurement returns JoinHandle<()> for independent caller cancellation and waits for one eligible peer before its single request. Retain and abort that handle when independent ownership ends.
* PeerGateway::new and GatewayObserver::new take a final Option<Sessions> coordinator; session-handled results require Some sharing the peer registry. Public schedule_remeasurement is one-shot.
* PeerGateway::new and GatewayObserver::new take a final Option<Sessions> coordinator; session-handled results require Some sharing the peer registry. Public schedule_remeasurement is one-shot.
* In addition to Measurement::new returning io::Result and the Error::Protocol variant, preserve PeerStateMessageType expires_at and observation metadata, handle PeerEvent::Observed and zero-TTL managed BYEBYE, construct Session with Session::new, and carry SessionKey through measurement requests/results and standalone measurement/result helpers. Handle MeasurePeerEvent::Cancelled as incarnation-scoped retryable lease retirement.
* PeerStateMessageType adds observation metadata alongside expires_at; PeerEvent adds Observed and managed BYEBYE becomes a zero-TTL peer-state withdrawal. Construct Session with Session::new; measurement requests/results and measure_peer, measure_peer_with_socket, handle_successful_measurement and handle_failed_measurement now carry SessionKey. Handle the new MeasurePeerEvent::Cancelled variant and preserve originating keys and observations when forwarding.
* **discovery:** PeerStateMessageType struct literals now require expires_at. Use None for standalone call-time TTL or preserve Some(original receive deadline) through managed forwarding.
* **link:** construct Session values with Session::new; incarnation identity is private and reset_session always creates a fresh incarnation.
* Measurement::new returns io::Result<Measurement>, and the public Error enum now includes Protocol. Downstream constructors and exhaustive matches must handle these cases.
* **audio:** stop audio sharing when Link is disabled
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
* own Link IO execution and complete core shutdown ([7bd0d08](https://github.com/anweiss/ableton-link-rs/commit/7bd0d08d282abcfd85b8a4324af25605fe0263b4)), closes [#106](https://github.com/anweiss/ableton-link-rs/issues/106) [#120](https://github.com/anweiss/ableton-link-rs/issues/120)
* **platform:** add ThreadPriority API (partial port; IO-thread wiring still outstanding) ([#137](https://github.com/anweiss/ableton-link-rs/issues/137)) ([15443b3](https://github.com/anweiss/ableton-link-rs/commit/15443b3a81717a3a9332a879e734d007effa5dbb))


### Bug Fixes

* acquire every session publication guard before committing ([be5aa3d](https://github.com/anweiss/ableton-link-rs/commit/be5aa3ddec157ea9e2b3951714ca49bc7164049b))
* add SessionId to ClientState for correct grace-period propagation ([#83](https://github.com/anweiss/ableton-link-rs/issues/83)) ([fd6fdd4](https://github.com/anweiss/ableton-link-rs/commit/fd6fdd41f316e1dd06e9ccc5aab07565cfbbbd86))
* **audio:** decode Ableton Live audio buffers and stabilize playback ([#72](https://github.com/anweiss/ableton-link-rs/issues/72)) ([10f0725](https://github.com/anweiss/ableton-link-rs/commit/10f0725da4d9530b372142ab2304cdd644b7b15a))
* **audio:** stop audio sharing when Link is disabled ([89122a2](https://github.com/anweiss/ableton-link-rs/commit/89122a20636512b51ed0d7c8a9e235910cf78b29))
* bind complete owned broadcasts to their discovery epoch ([5d3694d](https://github.com/anweiss/ableton-link-rs/commit/5d3694d91a2e5dbbfd5c75dc05cea08bf627772e))
* bound audio captures and retain wide tempo reference beats ([5264fb1](https://github.com/anweiss/ableton-link-rs/commit/5264fb1fe5ebe301726d139f955c3c26a0083ef1))
* carry discovery and measurement origins through final commits ([12e501b](https://github.com/anweiss/ableton-link-rs/commit/12e501b2d86016d8072471d8f92ee8050b217a7e))
* catch message-encode errors in sendUdpMessage ([#124](https://github.com/anweiss/ableton-link-rs/issues/124)) ([ad928c1](https://github.com/anweiss/ableton-link-rs/commit/ad928c19f65d9955992f26149f18930e9bc16fa3))
* **ci:** address the Copilot review findings from [#101](https://github.com/anweiss/ableton-link-rs/issues/101) ([#114](https://github.com/anweiss/ableton-link-rs/issues/114)) ([4dc1b1a](https://github.com/anweiss/ableton-link-rs/commit/4dc1b1ae4a1d78049bbb78f2be52ace1d79586dc))
* **ci:** announce a review-loop handoff instead of editing a sticky comment ([#155](https://github.com/anweiss/ableton-link-rs/issues/155)) ([7aff7c4](https://github.com/anweiss/ableton-link-rs/commit/7aff7c40e3cb0368d6bf0534a04b4680dbdfb7bf))
* **ci:** bound upstream watch triage and use system Python ([#166](https://github.com/anweiss/ableton-link-rs/issues/166)) ([9bedc37](https://github.com/anweiss/ableton-link-rs/commit/9bedc37e5c1a8aae1e6a8e409351b5b1d8ecffad))
* **ci:** close the three gaps that made the review loop need a human ([#123](https://github.com/anweiss/ableton-link-rs/issues/123)) ([e7166a1](https://github.com/anweiss/ableton-link-rs/commit/e7166a159d68228891d1df067409afac4d0f1429))
* **ci:** close three narrative-drift gaps the first unblocked port run exposed ([#148](https://github.com/anweiss/ableton-link-rs/issues/148)) ([d8764d1](https://github.com/anweiss/ableton-link-rs/commit/d8764d1f39e6d67c54b2de2cc37d51760ee73e5c))
* **ci:** drop unsupported $schema key from release-please manifest ([#194](https://github.com/anweiss/ableton-link-rs/issues/194)) ([937bbe6](https://github.com/anweiss/ableton-link-rs/commit/937bbe6fa57a78252554ea2ee0f72adf8be80bfd))
* **ci:** key the blocker exemption on a marker and stop using gh pr list ([#151](https://github.com/anweiss/ableton-link-rs/issues/151)) ([9fdd985](https://github.com/anweiss/ableton-link-rs/commit/9fdd9850630781bab102328a9605d7b89cefe025))
* **ci:** let the merge close the port item's tracking issue ([#133](https://github.com/anweiss/ableton-link-rs/issues/133)) ([42afc66](https://github.com/anweiss/ableton-link-rs/commit/42afc665077af480161a79050492f005382c0cd8))
* **ci:** let the upstream workflows actually write the backlog file ([#118](https://github.com/anweiss/ableton-link-rs/issues/118)) ([9b88ccd](https://github.com/anweiss/ableton-link-rs/commit/9b88ccdd1e68b7e55bd160102322343f77e0d29d))
* **ci:** migrate to AsyncFd::register, commit Cargo.lock, and repair agentic workflows ([6fa2263](https://github.com/anweiss/ableton-link-rs/commit/6fa2263149f19132bd87d8880839f0cfa894a009))
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
* clarify queue availability and feature migration ([b525bcb](https://github.com/anweiss/ableton-link-rs/commit/b525bcb2aa545a4cd218263662e762912adfbb5e))
* close dispatch admission and cancel owned tasks on controller drop ([#156](https://github.com/anweiss/ableton-link-rs/issues/156)) ([4ee4f6d](https://github.com/anweiss/ableton-link-rs/commit/4ee4f6dce475cdf1673ce000887b954376febe45))
* complete measurement failure results and cover hardening boundaries ([1a6b0a1](https://github.com/anweiss/ableton-link-rs/commit/1a6b0a1b7bf6b10be7aeceb77bfd88af9d1b825e))
* complete missing-endpoint construction before binding sockets ([2900d1b](https://github.com/anweiss/ableton-link-rs/commit/2900d1b01cd42876ed913d2584698976e700b5d1))
* coordinate peer retirement and bounded measurement ownership ([661b9d4](https://github.com/anweiss/ableton-link-rs/commit/661b9d4084b675a0342e79bdfbade3881c696f6b))
* coordinate session admission and clock publication ([746b901](https://github.com/anweiss/ableton-link-rs/commit/746b901c5f2c644063b6f25d2ac0652a6bc777b9))
* correct audio chunk resizer packet sizing and timing-change detection ([#176](https://github.com/anweiss/ableton-link-rs/issues/176)) ([7fddef0](https://github.com/anweiss/ableton-link-rs/commit/7fddef0304a424eea57cf146313f701b3b00f29a)), closes [#175](https://github.com/anweiss/ableton-link-rs/issues/175)
* decouple discovery handoff and preserve public measurement completions ([1193367](https://github.com/anweiss/ableton-link-rs/commit/1193367c8f629a4f34d8534454a7ce5c687325c5))
* deliver only joined multicast groups to the discovery sockets on Linux ([#152](https://github.com/anweiss/ableton-link-rs/issues/152)) ([ed0e4ec](https://github.com/anweiss/ableton-link-rs/commit/ed0e4ecfba414624e476f8c4ce6f7dd4aced89c3))
* **discovery:** avoid port sharing for ephemeral UDP sockets ([#167](https://github.com/anweiss/ableton-link-rs/issues/167)) ([9c57237](https://github.com/anweiss/ableton-link-rs/commit/9c572378437e7da63dfb15885ec1b849d142a004))
* **discovery:** filter disconnected adapters in public scanners ([db4ac3e](https://github.com/anweiss/ableton-link-rs/commit/db4ac3e9a7ff5493c6b9f90b360400ae98dad6e4))
* **discovery:** inherit post-lock receive deadline validation ([340d954](https://github.com/anweiss/ableton-link-rs/commit/340d954f594963d81af351ef43f87f977311b5eb))
* **discovery:** invalidate queued revisions beyond route-hint lifetime ([e9bfd3e](https://github.com/anweiss/ableton-link-rs/commit/e9bfd3e5ef3f5769ce1d3e42f4f789740988b53b))
* **discovery:** measure peers on every interface, and keep IPv4 link-local ([8907edf](https://github.com/anweiss/ableton-link-rs/commit/8907edf63dd9cd6dde23fa3dd34c49a4830e097f))
* **discovery:** preserve received lifetimes and cancel retired routes ([3d29287](https://github.com/anweiss/ableton-link-rs/commit/3d292879a184406b84e80c211bd739b701f7cc82))
* **discovery:** recheck receive deadlines after timeout lock waits ([fa254c0](https://github.com/anweiss/ableton-link-rs/commit/fa254c0dda75f15040e1e38a670242680a18f0fe))
* **discovery:** reject unknown message types before they're looked up ([eb65273](https://github.com/anweiss/ableton-link-rs/commit/eb6527384d9297e39eb32077c639704d6e81bb13))
* **discovery:** retain ingress leases through bounded event delivery ([f8f60a3](https://github.com/anweiss/ableton-link-rs/commit/f8f60a3b141bb1e3e90c02029160c569b7f4dd38))
* **discovery:** skip adapters that are down, warn once per failing interface ([4931438](https://github.com/anweiss/ableton-link-rs/commit/4931438c3074b22cadf1fa547facf24bdbc9faad))
* distinguish malformed Link protocol data from truncated input ([16b888b](https://github.com/anweiss/ableton-link-rs/commit/16b888bf4dd6d781a0b84e8452a3c25618c93b5b))
* enforce safe-by-default with deny(unsafe_code) instead of asking ([#141](https://github.com/anweiss/ableton-link-rs/issues/141)) ([6ff330b](https://github.com/anweiss/ableton-link-rs/commit/6ff330b9fc996c5d4173509fb19a69e19d5538df))
* explicitly skip aep6 payload entries for IPv6 audio endpoints ([#86](https://github.com/anweiss/ableton-link-rs/issues/86)) ([4f52e8f](https://github.com/anweiss/ableton-link-rs/commit/4f52e8fc1a67795cccb687cfa89a124f0e98c3f8))
* fail missing multicast and invalid fixture expectations ([be6e309](https://github.com/anweiss/ableton-link-rs/commit/be6e3097cb0209bbd74324697fa371ee691c1d5e))
* gate Controller dispatch loops on disable to stop callbacks after shutdown ([#147](https://github.com/anweiss/ableton-link-rs/issues/147)) ([78dc501](https://github.com/anweiss/ableton-link-rs/commit/78dc5017ae6d14cb7873d78458da6efecda414dd))
* gate controller-only clock adapters to std and tests ([2fd2415](https://github.com/anweiss/ableton-link-rs/commit/2fd241578a6f524846b78e3fec235e9525763af0))
* integrate accepted-current timeline provenance ([5c16fbb](https://github.com/anweiss/ableton-link-rs/commit/5c16fbbe9efa4aeda0b5f9f77bf1ced10eecc9cd))
* integrate atomic admission and clock publication ([dc13933](https://github.com/anweiss/ableton-link-rs/commit/dc1393319cf45b16fa8149c9cd5161d3720e7216))
* integrate checked atomic compatibility ([430c59d](https://github.com/anweiss/ableton-link-rs/commit/430c59d0ff083a0ce448c2330812215bbe996c28))
* integrate checked session commits into protocol hardening ([ec1c56c](https://github.com/anweiss/ableton-link-rs/commit/ec1c56c6d492d5e080b0aefe73d1ec501b63aa4c))
* integrate cooperative discovery socket and lifecycle boundaries ([1c230a8](https://github.com/anweiss/ableton-link-rs/commit/1c230a8d91cca97e0daf0ae1601aa57d0202d06a))
* integrate cooperative registry and measurement ownership ([ad9c3f1](https://github.com/anweiss/ableton-link-rs/commit/ad9c3f1d997a7612a76cff63ce4d742492d51182))
* integrate coordinated peer and measurement ownership ([f181236](https://github.com/anweiss/ableton-link-rs/commit/f1812362e2abe616ca4af5969792c58a8c9e5541))
* integrate deadlock-free observer identity snapshot ([f0c4fda](https://github.com/anweiss/ableton-link-rs/commit/f0c4fda7505c1823eb880a0ad9d82c0c10787bf6))
* integrate durable notifications and queue-scoped tickets ([1a5f0f4](https://github.com/anweiss/ableton-link-rs/commit/1a5f0f4c1b207bc7acd4f0b51db2e9edffb7d30c))
* integrate duration-boundary arithmetic into protocol hardening ([a4b57e3](https://github.com/anweiss/ableton-link-rs/commit/a4b57e3ed0075d6318e84d8529f9bd7a71f3eb29))
* integrate effective quantum-boundary re-anchoring ([6b408cc](https://github.com/anweiss/ableton-link-rs/commit/6b408ccace936b876ce472f881c4506ad4fe06ea))
* integrate end-to-end discovery and session origins ([7c7a421](https://github.com/anweiss/ableton-link-rs/commit/7c7a4212ea646b2785896f41c097ee91bac2b203))
* integrate exact clamped anchor validation ([492793a](https://github.com/anweiss/ableton-link-rs/commit/492793a8a0142d315cb77171b1313cbc44c8beae))
* integrate exact clock arithmetic and migration organization ([496d1fa](https://github.com/anweiss/ableton-link-rs/commit/496d1fa39dd70de0b606bd67ff232a93c9df6864))
* integrate exact phase anchors and alignment retries ([f0476bb](https://github.com/anweiss/ableton-link-rs/commit/f0476bb57d3f9bef40fa7733d3688e88ca71090d))
* integrate final cleanup and candidate rearm boundaries ([fec65a6](https://github.com/anweiss/ableton-link-rs/commit/fec65a64147e5d249916ec796b13ca1507f266ab))
* integrate fully guarded session publication ([74408de](https://github.com/anweiss/ableton-link-rs/commit/74408de34b0270426cb9eaec76d52c91edb2d3ad))
* integrate managed wire-origin rejection and test-only dependency ([0477185](https://github.com/anweiss/ableton-link-rs/commit/0477185580159bd44b42638a4a42dc7f420d6742))
* integrate nanosecond forced-beat anchors ([96ef032](https://github.com/anweiss/ableton-link-rs/commit/96ef032f44752a52921ef09bd16092ed4af451a9))
* integrate nanosecond frame and inverse conversion ([38cd4fc](https://github.com/anweiss/ableton-link-rs/commit/38cd4fcbced2bdda3fe452bad900a9e31fea37c0))
* integrate owned publication into packet hardening ([1cc22b4](https://github.com/anweiss/ableton-link-rs/commit/1cc22b4ec64b430c6ae8ef2daad35eb7625c420a))
* integrate queue and feature migration documentation ([174f4c8](https://github.com/anweiss/ableton-link-rs/commit/174f4c87159752e086b6edce65dd6cbfdfae24f1))
* integrate recoverable audio commit tickets ([195caab](https://github.com/anweiss/ableton-link-rs/commit/195caab656659299f382c1ebcea082f9c3f03829))
* integrate shared drains and value-only capture provenance ([82af48c](https://github.com/anweiss/ableton-link-rs/commit/82af48c2ec158e1d5ea4cd8a8f9f727a4aecded0))
* integrate sign-aware LinkAudio period encoding ([9f737a1](https://github.com/anweiss/ableton-link-rs/commit/9f737a18c3229d5ec6b71d38ef34bb94ce13b35e))
* integrate the final forced-beat arithmetic guard ([39fc738](https://github.com/anweiss/ableton-link-rs/commit/39fc7381c053fe9309ab416d4d9952a49ec05702))
* integrate wide periods and cooperative measurement completion ([146f6ed](https://github.com/anweiss/ableton-link-rs/commit/146f6ed7c02fed3e0a85f07b1a4c013ccd172989))
* integrate wide phase and cleanup ownership repairs ([23ddc46](https://github.com/anweiss/ableton-link-rs/commit/23ddc466d0f6d2c811c6493d47887eefbde3461c))
* integrate wide timing and cancellable discovery publication ([8eb3cf2](https://github.com/anweiss/ableton-link-rs/commit/8eb3cf2b11b3de9a73a897d42e8aa626f774d8a4))
* invalidate ingress leases on raw topology changes ([#162](https://github.com/anweiss/ableton-link-rs/issues/162)) ([aa677b7](https://github.com/anweiss/ableton-link-rs/commit/aa677b72cd5fbf27613725bb0c739c1f38582ddb))
* keep audio provenance value-only and serialize accepted delivery ([c2ac97b](https://github.com/anweiss/ableton-link-rs/commit/c2ac97bf0c8045370f1524d3ff4e803dfee8e2c2))
* keep coupled discovery IO acquisitions cooperative ([e651030](https://github.com/anweiss/ableton-link-rs/commit/e6510303713ae480c187c0192e73bc06fe1d0dcd))
* keep measurement completion nonblocking under result backpressure ([ceb2549](https://github.com/anweiss/ableton-link-rs/commit/ceb25492b8b8808694458b2cc6e438a1705c6b16))
* keep unity clock conversion exact across duration range ([d196828](https://github.com/anweiss/ableton-link-rs/commit/d1968281bcc417a1b6abf1aea701a3d30b9c0430))
* **link:** commit coherent session state and honor constructor cancellation ([4cb0e5d](https://github.com/anweiss/ableton-link-rs/commit/4cb0e5dc5777f7be3b684398ba783e68c2b241d9))
* **link:** reject stale session incarnations and partial PONG samples ([554ebf9](https://github.com/anweiss/ableton-link-rs/commit/554ebf9141b2188ffd7d72fe6b489d5e06d1097d))
* make registry contention cooperative and retain measurement ownership ([295f310](https://github.com/anweiss/ableton-link-rs/commit/295f310591f1681101a48a918443d73a7fb4ab90))
* **no_std:** build the lock helpers only with std; document the panic policy ([ca8f4b6](https://github.com/anweiss/ableton-link-rs/commit/ca8f4b67c60e3262d0b59b4983518a87f6b7aa4b))
* **payload:** check declared entry lengths before using them ([6a50f45](https://github.com/anweiss/ableton-link-rs/commit/6a50f4552d88ebf339a5b2ead8a022521dd5cfd0))
* **payload:** don't announce, or panic on, a missing measurement endpoint ([1b83a85](https://github.com/anweiss/ableton-link-rs/commit/1b83a85edae1bafd06775515ca6b7d0974019ca3))
* preserve absent endpoints and route measurements through discovery ingress ([36c4029](https://github.com/anweiss/ableton-link-rs/commit/36c4029d5ddabb21aeb07540bac14a66bc055a6e))
* preserve checked atomic updates across supported Rust versions ([316b7f4](https://github.com/anweiss/ableton-link-rs/commit/316b7f41d1224fda19034290d0e6653db9b44e8e))
* preserve cleanup and candidate rearm at final boundaries ([3829a1d](https://github.com/anweiss/ableton-link-rs/commit/3829a1d78caf3665f987ff0b450de44ada5ebf53))
* preserve clock precision through scaling and cancellation ([a9b5dbe](https://github.com/anweiss/ableton-link-rs/commit/a9b5dbed72e6de863a7f07b779fa6fa60d2b7d09))
* preserve fractional alignment through timeline publication ([7c1b2e0](https://github.com/anweiss/ableton-link-rs/commit/7c1b2e07986f44332dca03710b73e983f57d522f))
* preserve hardening while adopting broadcast epoch cancellation ([6bd83ef](https://github.com/anweiss/ableton-link-rs/commit/6bd83efb66a99186dc390c7319ddc00ce2fa6e0e))
* preserve missing endpoint failures with uncertain NIC evidence ([0fde705](https://github.com/anweiss/ableton-link-rs/commit/0fde705b9775d6e2e71312fa6befb6da1013ac90))
* preserve representable clock and timeline boundary results ([d66cc9f](https://github.com/anweiss/ableton-link-rs/commit/d66cc9fbfa986405374b0099681012e55677a260))
* preserve the sign of wide LinkAudio wire periods ([e01ef1a](https://github.com/anweiss/ableton-link-rs/commit/e01ef1a36d17ab3463f68ca2798a2585cbde89a8))
* preserve wide beat periods and cooperative measurement completion ([772a45b](https://github.com/anweiss/ableton-link-rs/commit/772a45b748ab2647e7739a0bfbf9c35975497208))
* preserve wide phase composition and cancellation-safe cleanup ([befffac](https://github.com/anweiss/ableton-link-rs/commit/befffac3762d23fbe033ba6622e2545935ba4bf1))
* preserve wide timing through cancellable wire publication ([29f509b](https://github.com/anweiss/ableton-link-rs/commit/29f509b901d9a86fc56ec083a1f5d6acd4f1b6ed))
* publish received state through the owning session boundary ([5e54eed](https://github.com/anweiss/ableton-link-rs/commit/5e54eedf3681f996b7789bee638db163bf5c2cff))
* publish timeline provenance from the accepted current session ([1b5f21a](https://github.com/anweiss/ableton-link-rs/commit/1b5f21ade2a3590531256d3e0a0912481172e19b))
* recover outstanding audio commit tickets without consuming results ([74fb0aa](https://github.com/anweiss/ableton-link-rs/commit/74fb0aad74fc5462286ee63fa910f0a0b9deb5e1))
* reject exhausted and stale session commits atomically ([3a12636](https://github.com/anweiss/ableton-link-rs/commit/3a126368f498bf9cab363b6f57bb751afc4cf398))
* reject leaked and ambiguous NIC capture evidence ([863be26](https://github.com/anweiss/ableton-link-rs/commit/863be26b381ad93538fedb77a7811768528c275a))
* reject malformed LinkAudio buffer messages and zero sample rate commits ([#180](https://github.com/anweiss/ableton-link-rs/issues/180)) ([22d8784](https://github.com/anweiss/ableton-link-rs/commit/22d8784a34986bcfd24d844cd412dd26fcb8b60b)), closes [#179](https://github.com/anweiss/ableton-link-rs/issues/179)
* reject managed origins outside wire range ([b8d864c](https://github.com/anweiss/ableton-link-rs/commit/b8d864c4d3be4a2a34bee26af25c4be388234e3f))
* reject missing measurement endpoints before allocating sockets ([e0381a5](https://github.com/anweiss/ableton-link-rs/commit/e0381a5f8fb27dd50e3d938b30447d866c38e071))
* reject multicast egress on known foreign NICs ([60b1b65](https://github.com/anweiss/ableton-link-rs/commit/60b1b657cc7140202befc7ae46eb11bf010be1af))
* reject phase-changing timeline compositions ([2d3ae54](https://github.com/anweiss/ableton-link-rs/commit/2d3ae5429da54741e702dcf5cca7cb9258f44919))
* release observer identity before ingress arbitration ([2704bfa](https://github.com/anweiss/ableton-link-rs/commit/2704bfac5fed2ab767513c031bf48bbc753679c5))
* **release:** track README dependency versions automatically ([4b78f07](https://github.com/anweiss/ableton-link-rs/commit/4b78f070b6e83fa1f99d6bf60645b3dfed6f3cea))
* **release:** track README dependency versions automatically ([a7a04bb](https://github.com/anweiss/ableton-link-rs/commit/a7a04bbca6e7d586c8105cf77ab52ad072880c55))
* remove ambiguous session drains and preserve unchanged tempo ([12e3864](https://github.com/anweiss/ableton-link-rs/commit/12e3864eca2963660ce726d7f2c60c7f8e276e44))
* replace peer timeouts atomically and retain poison recovery ([b82e427](https://github.com/anweiss/ableton-link-rs/commit/b82e427a686154724c80da7c98b7f206f344ddee))
* restore measurement and discovery across enable cycles ([#158](https://github.com/anweiss/ableton-link-rs/issues/158)) ([5da0685](https://github.com/anweiss/ableton-link-rs/commit/5da0685c50e2d83701024a5d25625dc113b19216))
* retain effective quantum boundaries when re-anchoring ([3d6216b](https://github.com/anweiss/ableton-link-rs/commit/3d6216b0dc4e684883d7441eca2d763e54923d16))
* retain exact clock precision and canonical migration notes ([567284f](https://github.com/anweiss/ableton-link-rs/commit/567284f8fe55d16817dcd6ae96e6fa89796c531c))
* retain fractional session zero anchors ([527ac0c](https://github.com/anweiss/ableton-link-rs/commit/527ac0cfdeeb418995b5bec42c02d7ffbe3433ae))
* retain ingress interface identity for discovery responses ([#161](https://github.com/anweiss/ableton-link-rs/issues/161)) ([8044864](https://github.com/anweiss/ableton-link-rs/commit/8044864ec373263104f58e30ec9655d38284843e))
* retain nanosecond precision in forced-beat anchors ([87c860d](https://github.com/anweiss/ableton-link-rs/commit/87c860d1f874ec4fcc4c72bbe30f4d7eccab0147))
* retain tempo delivery and scope audio completion tickets ([399f3e2](https://github.com/anweiss/ableton-link-rs/commit/399f3e25c9e8baa277d05b214fe566aaeaa2a314))
* retry playback alignment and solve fast phase anchors ([a362e06](https://github.com/anweiss/ableton-link-rs/commit/a362e06a9b016872d34de52e8ff3e60d2d271703))
* return an error instead of panicking on oversized ping/pong messages ([#144](https://github.com/anweiss/ableton-link-rs/issues/144)) ([88ad7ce](https://github.com/anweiss/ableton-link-rs/commit/88ad7ce3c424468de6ef6c5937f8425a617c441c))
* saturate zero-period forced beat composition ([368e23d](https://github.com/anweiss/ableton-link-rs/commit/368e23d2c1e7ef9490a89fd850b2d77eb0616284))
* saturate zero-period forced beat composition ([db38ee7](https://github.com/anweiss/ableton-link-rs/commit/db38ee7c98970566202825b60c0dd8e509f9863b))
* scope the stranded-port detector to this workflow's own output ([#75](https://github.com/anweiss/ableton-link-rs/issues/75)) ([53f97b9](https://github.com/anweiss/ableton-link-rs/commit/53f97b911a26fb74334780c43e308cd564925ea1))
* send managed measurement replies from advertised endpoints ([fc79804](https://github.com/anweiss/ableton-link-rs/commit/fc79804066cd711db05560c1238a88c736ae3c97))
* stabilize LinkAudio example playback ([#206](https://github.com/anweiss/ableton-link-rs/issues/206)) ([ef0d967](https://github.com/anweiss/ableton-link-rs/commit/ef0d9672d8eb0ef11fcaa8506166d1dbdae2d358)), closes [#202](https://github.com/anweiss/ableton-link-rs/issues/202) [#203](https://github.com/anweiss/ableton-link-rs/issues/203) [#204](https://github.com/anweiss/ableton-link-rs/issues/204) [#205](https://github.com/anweiss/ableton-link-rs/issues/205)
* stream Windows fixture diagnostics while retaining markers ([e2b5998](https://github.com/anweiss/ableton-link-rs/commit/e2b599801e19dd3b8996e573ba885a74f149d879))
* strengthen discovery ingress platform evidence ([4271835](https://github.com/anweiss/ableton-link-rs/commit/42718354cc07ec4f9072bec096934d73a8414479))
* strengthen discovery ingress platform evidence ([06d8e90](https://github.com/anweiss/ableton-link-rs/commit/06d8e90256f5c2294183b8932116f6927d35d9b0))
* support native pktmon records and writable topic docs ([1b7a43e](https://github.com/anweiss/ableton-link-rs/commit/1b7a43ed583e9df4825072de3f361801cbed1a88))
* tear down LinkAudio before Link on drop (SessionController shutdown ordering) ([#119](https://github.com/anweiss/ableton-link-rs/issues/119)) ([8446299](https://github.com/anweiss/ableton-link-rs/commit/8446299760bd165bd00bfb5737a3df54d841e589))
* **topology:** register the route socket with AsyncFd::register ([76e1a31](https://github.com/anweiss/ableton-link-rs/commit/76e1a3131453741dad928ad620799f08096f766f))
* validate clamped nanosecond anchor candidates ([0432ba1](https://github.com/anweiss/ableton-link-rs/commit/0432ba1b455802e70cc249cbfbbee8f695dbf229))


### Code Refactoring

* share the unicast socket across measurements in MeasurementService ([#76](https://github.com/anweiss/ableton-link-rs/issues/76)) ([e9f978c](https://github.com/anweiss/ableton-link-rs/commit/e9f978c5c6606e6de9fed48beaa3d01d106ce4c5))


### Documentation

* [Copilot Dreams] prevent incorrect release and audio-format assumptions ([78358c0](https://github.com/anweiss/ableton-link-rs/commit/78358c0bc8d33d275cb058c517e93b82b7d7f59b))
* [Copilot Dreams] prevent incorrect release and audio-format assumptions ([365a977](https://github.com/anweiss/ableton-link-rs/commit/365a977abc85689aab0bf49d4f884dc86d788578))
* align contributor checklist and topic navigation ([85d96d1](https://github.com/anweiss/ableton-link-rs/commit/85d96d12f8415b1001531eda23dbdbc71f751b72))
* **ci:** make a retire-only port PR close its tracking issue too ([#134](https://github.com/anweiss/ableton-link-rs/issues/134)) ([f5924c4](https://github.com/anweiss/ableton-link-rs/commit/f5924c41beed7b0098e04a0ae6cadd9c584d02f2))
* **ci:** split backlog prose by audience so generated issues read as English ([#150](https://github.com/anweiss/ableton-link-rs/issues/150)) ([7447367](https://github.com/anweiss/ableton-link-rs/commit/7447367f8553c0a469d173bbccce708845f64877))
* **discovery:** correct registration safety and fixture evidence ([ceab210](https://github.com/anweiss/ableton-link-rs/commit/ceab210b0269c0a3e9b7372aa4fb133aac13b6b1))
* distinguish bounded discovery delivery and public result backpressure ([8efdbb2](https://github.com/anweiss/ableton-link-rs/commit/8efdbb219adb7fa43a2e75d65b73a8ad4356ab38))
* distinguish saturating timestamps from wire-time errors ([076e03a](https://github.com/anweiss/ableton-link-rs/commit/076e03aac98dbcbb43ec3e25e394519406b2ad68))
* keep LinkAudio guidance numbering contiguous ([608762d](https://github.com/anweiss/ableton-link-rs/commit/608762db7bebbbdbf70a3c491b77e5bb60978e18))
* qualify --locked claim in README ([#196](https://github.com/anweiss/ableton-link-rs/issues/196)) ([ca4ace4](https://github.com/anweiss/ableton-link-rs/commit/ca4ace4e75e848153e788870a8f893264cebf5ee))
* **readme:** describe the file-based porting backlog ([#116](https://github.com/anweiss/ableton-link-rs/issues/116)) ([06e8314](https://github.com/anweiss/ableton-link-rs/commit/06e8314af629510372922dc24353c950c1c1d3dd))
* slim README and organize detailed project guidance ([d6d0624](https://github.com/anweiss/ableton-link-rs/commit/d6d062495da6777b815f4b6d47d1b4ba9946e931))


### Tests

* assert precise wide start-stop conversion ([34b7256](https://github.com/anweiss/ableton-link-rs/commit/34b7256f836e871b2204c613d00b69d053584d8b))
* cover Messenger measurement setup across registration churn ([1ce8062](https://github.com/anweiss/ableton-link-rs/commit/1ce8062cfc6a18b5b55baf7b60992996b4073823))
* cover packet compatibility and clarify snapshot publication ordering ([b7138a5](https://github.com/anweiss/ableton-link-rs/commit/b7138a5914fa16535a5e1d8ea8f0848852508e3b))
* cover saturation boundaries and clarify local protocol errors ([beab674](https://github.com/anweiss/ableton-link-rs/commit/beab6746102896ffb8d657e9221ec43e2499397c))
* decouple lifecycle measurement fixtures from host multicast routes ([2cb9ca6](https://github.com/anweiss/ableton-link-rs/commit/2cb9ca658196eee8d899844c5b1d1fc967edcf2d))
* exercise repeated accepted audio edits ([94e5a8b](https://github.com/anweiss/ableton-link-rs/commit/94e5a8b639ec28ac2a4cd86c1150d11e866db546))
* format exact start-stop boundary assertion ([cf2ea7c](https://github.com/anweiss/ableton-link-rs/commit/cf2ea7c8689bfa48f1418ecfeaf69554307b9476))
* integrate corrected link-helper reference ([662759e](https://github.com/anweiss/ableton-link-rs/commit/662759e9e2338186d9dfb76910567c7a811a7adb))
* integrate exact wide start-stop expectation ([3304e79](https://github.com/anweiss/ableton-link-rs/commit/3304e79e431622778f2a77f81368ae94dde5779b))
* integrate formatted boundary assertion ([2fcf5f2](https://github.com/anweiss/ableton-link-rs/commit/2fcf5f23a21fcad7a319d12b26d456b1747a6284))
* keep Windows measurement source probes within loopback ([17dd33f](https://github.com/anweiss/ableton-link-rs/commit/17dd33f0c549ef0a6384ea41a66e3c3573cb26fe))
* **link:** assert cancelled joins stay uncommitted under backpressure ([574904f](https://github.com/anweiss/ableton-link-rs/commit/574904f495abc766c5afdc7b36d27d63f6fab889))
* reference the link duration helper in boundary assertion ([1ff55a5](https://github.com/anweiss/ableton-link-rs/commit/1ff55a53d703e9a0efe78ea917f13e8678a3a99b))
* retain deadline metadata in poisoned gateway fixture ([5b85f6d](https://github.com/anweiss/ableton-link-rs/commit/5b85f6d809ea22cd65cc2fd1e82902afbec2b6b2))


### CI/CD

* auto-merge upstream port PRs once required checks pass ([#77](https://github.com/anweiss/ableton-link-rs/issues/77)) ([0e2042a](https://github.com/anweiss/ableton-link-rs/commit/0e2042a72a2523049440d66060c21db7bb66388c))
* automate upstream triage reviews and merges ([#178](https://github.com/anweiss/ableton-link-rs/issues/178)) ([a08f797](https://github.com/anweiss/ableton-link-rs/commit/a08f797c361fe3e0b4b7331a5354864dff18cf1e))
* **deps:** bump actions/create-github-app-token from 3.2.0 to 3.3.0 ([cd8a941](https://github.com/anweiss/ableton-link-rs/commit/cd8a9412621e72945aaf9986f19d18fd3f195f51))
* **deps:** bump actions/create-github-app-token from 3.2.0 to 3.3.0 ([80796e8](https://github.com/anweiss/ableton-link-rs/commit/80796e8fff40c0e7b94fcd9882ee0b59bb4421ba))
* **deps:** bump actions/download-artifact from 8.0.1 to 8.0.2 ([9f11855](https://github.com/anweiss/ableton-link-rs/commit/9f118558fd251a63b38452b9dbf0185a87ab98a7))
* **deps:** bump actions/download-artifact from 8.0.1 to 8.0.2 ([14ebe77](https://github.com/anweiss/ableton-link-rs/commit/14ebe7723b32008c104cc2289ce134aba02fce3c))
* **deps:** bump actions/upload-artifact from 7.0.1 to 7.0.2 ([6bbe6eb](https://github.com/anweiss/ableton-link-rs/commit/6bbe6ebd2b70eff9a4c1897992e84272277a57e0))
* **deps:** bump actions/upload-artifact from 7.0.1 to 7.0.2 ([c685215](https://github.com/anweiss/ableton-link-rs/commit/c685215fc91ecf6d7027b90ce68f750c6e618431))
* **deps:** bump github/gh-aw-actions/setup from 0.86.2 to 0.87.0 ([#82](https://github.com/anweiss/ableton-link-rs/issues/82)) ([0eb1fe4](https://github.com/anweiss/ableton-link-rs/commit/0eb1fe4c6faa9e107894e8c6cb5868a31f559823))
* **deps:** bump github/gh-aw-actions/setup from 0.87.0 to 0.87.4 ([#95](https://github.com/anweiss/ableton-link-rs/issues/95)) ([fe048fd](https://github.com/anweiss/ableton-link-rs/commit/fe048fd579aa8ae23a5c42f3ab682b41775b110b))
* **deps:** bump github/gh-aw-actions/setup-cli from 0.84.2 to 0.86.1 ([#74](https://github.com/anweiss/ableton-link-rs/issues/74)) ([b3eb2ca](https://github.com/anweiss/ableton-link-rs/commit/b3eb2ca766baebede2a8e19a5f8d190ee0607d45))
* **deps:** bump github/gh-aw-actions/setup-cli from 0.86.2 to 0.87.0 ([#81](https://github.com/anweiss/ableton-link-rs/issues/81)) ([627f92a](https://github.com/anweiss/ableton-link-rs/commit/627f92a20816a257856912a7d089f7ee817eb042))
* **deps:** bump github/gh-aw-actions/setup-cli from 0.87.0 to 0.87.4 ([#96](https://github.com/anweiss/ableton-link-rs/issues/96)) ([2ad027f](https://github.com/anweiss/ableton-link-rs/commit/2ad027fca5bce8ee9f6dc248abe7dc2d098f70a4))
* **deps:** update dtolnay/rust-toolchain requirement to 02cb101ec7c40f2c49e1d9714d64511d8e1b74de ([#174](https://github.com/anweiss/ableton-link-rs/issues/174)) ([9e79c0d](https://github.com/anweiss/ableton-link-rs/commit/9e79c0d94115b1f1ee24e591f22dc2449bce9526))
* enforce the README-maintenance rule instead of restating it ([#135](https://github.com/anweiss/ableton-link-rs/issues/135)) ([5c0cdc5](https://github.com/anweiss/ableton-link-rs/commit/5c0cdc5002dae5360893646e4a75af4f7ac9735b))
* recompile agentic workflow locks with gh-aw v0.86.2 ([#78](https://github.com/anweiss/ableton-link-rs/issues/78)) ([6cccd28](https://github.com/anweiss/ableton-link-rs/commit/6cccd2899dcd8dff4baa74f3bd69ade1f05f024e))

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
