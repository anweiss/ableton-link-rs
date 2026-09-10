# Owned execution and shutdown

`Controller::new` creates a named OS thread with a current-thread Tokio runtime,
then constructs the controller and its network resources on it. Construction
returns only after the result is received. Failure or cancellation drops the
executor owner, stops the runtime and joins the thread. Construction does not
require the caller's runtime to drive Link's startup.

The measurement consumer, session-result consumer, core dispatch loops and
discovery listener run on that runtime; their spawned children inherit it.
The listener first started by `enable` is explicitly spawned on the retained IO
handle, not on the caller's runtime. The runtime also owns its blocking pool
used for interface enumeration. Public standalone networking/measurement helpers
continue to use the runtime supplied by their caller.

## Priority

`BasicLink::set_io_thread_priority(bool)` and the corresponding Controller method
send a command to the owned thread. Ordinary scheduling remains the default.
Both promotion and restoration use the existing safe `audio_thread_priority`
backend, with its documented parameter differences from upstream. The command
returns the OS result, and a thread-local guard attempts restoration during
normal shutdown or unwind. No shared caller worker is boosted. Cancelling the
request future does not retract an already queued command; shutdown still
attempts restoration. Calls after shutdown return an error.

This is an IO-execution hook, not a claim about audio playback threads or
measured drift. `LinkAudio` exposes it through BasicLink; its separate audio
engine and peer-sync task share the owned IO executor. Standalone `AudioEngine`
instances remain caller-managed. The example opts in only when
`LINK_IO_REALTIME=1`. Existing ThreadPriority best-effort methods remain available,
alongside error-returning `try_set_high` / `try_reset`.

## Shutdown contract

External Controller drop:

1. Closes managed callback admission and the core/discovery dispatch gates.
2. Signals the owned runtime to stop.
3. Joins its OS thread. Runtime destruction cancels pending async work and waits
   for its blocking-pool work; a synchronous callback already executing is not
   preempted.
4. Releases the controller fields after the join.

The callback-entry check is the invocation admission boundary. The closure checks
the closed flag after acquiring its callback mutex and before calling user code.
Tempo callbacks retain their existing best-effort, nonblocking lock behavior:
contention (including recursive invocation) skips the callback rather than
blocking another executor or deadlocking on the same callback. External
drop cannot return while an admitted invocation is running. The state-update
helper releases session and registration locks before invoking user code;
callback serialization itself still uses the callback's public mutex. After
acquiring that mutex, a pending tempo notification is checked against current
session tempo; an overtaken notification is suppressed rather than delivered
after a newer value. The state check is also nonblocking and releases its guard
before invoking user code.

Reentrant drop cannot join the executing thread. It closes admission and signals
stop immediately, then transfers the thread handle to a retained process-wide
join service. The current callback may unwind/return; subsequent managed
invocations are rejected. The join service owns completion, including reporting
thread panic. Its service thread remains available for the process lifetime.
Application code must not block callbacks waiting on their own IO executor, or
hold locks during external drop that an executing callback needs to finish.

This contract covers core background tempo/audio-endpoint callbacks and
LinkAudio's background channel/source callbacks. Initial
registration callbacks and app-state callbacks remain synchronous calls by the
application. It does not rewrite standalone public helpers. LinkAudio closes
its engine before dropping the core; the core's join then waits for both audio
and core work, including the peer-sync task, before core fields are released.
Audio's existing bounded best-effort BYEBYE/state cleanup is not used as proof
of task completion. Reentrant audio teardown has the same one-invocation
exception as core teardown. Source admission is attached to the Source object,
so replacing its callback cannot remove the shutdown check. Channel publication
uses a nonblocking callback lock, including recursive shutdown publication.
`disable` remains asynchronous and restartable, retaining the
existing acknowledged gates, disabled drains and epoch cancellation.

BYEBYE cleanup constructs a nonblocking standard socket directly, so final drop
works on an ordinary thread after the caller's Tokio runtime has been destroyed.
Encoding and socket errors are logged; cleanup does not create a temporary reactor.

## Evidence and closure

Deterministic tests cover an executing callback held by an acknowledgment,
contended callback suppression, callback-initiated Controller drop,
owned-thread identity (including descendants), parked-task release, cross-runtime
loopback UDP, and real OS priority request/restore results. Existing restart and
measurement regressions remain required.

Real UDP audio-announcement regressions hold a receive callback with a channel
acknowledgment, verify its thread matches the core executor, and demonstrate
external drop waits on current-thread and multithread caller runtimes. Another
real receive callback drops LinkAudio itself and observes final core-resource
release. Restoring caller-runtime audio construction makes the current-thread
regression time out; restoring owned construction passes.

The Linux CI job additionally requires a successful priority request and reset
in a disposable privileged test process; its ordinary unprivileged serial run
also exercises permission denial. macOS and Windows execute the same native
request/reset test, reporting accepted or denied results rather than inferring
scheduler behavior from compilation. No latency or clock-drift improvement has
been measured.

The network-ingress contract and the remaining index-reuse/platform evidence
limits are separate: see [discovery ingress](discovery-ingress.md). Neither
owned execution nor a safe socket wrapper supplies an atomic OS interface
generation token. Do not close #154 based on this execution change.
