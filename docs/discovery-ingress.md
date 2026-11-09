# Discovery ingress identity

The managed `Messenger` used by `BasicLink` retains the receiving OS interface
index for both multicast announcements and unicast replies. It no longer guesses
the response interface from the sender's IP address. The wildcard listener still
joins the Link group on each usable IPv4 interface; the wire protocol, public
method signatures, feature names and Rust 1.87 library MSRV are unchanged.

## Why arrival metadata

The pinned upstream (`334972b404d7`) has a multicast listener and unicast socket
per `IpInterface`. Its `UdpMessenger` also applies a hard-coded `/24` source
filter as a Linux delivery workaround. Neither that filter nor the Rust port's
old longest-common-prefix selection proves the ingress adapter when subnets
overlap. Listener ownership alone is not a portable proof of delivery scoping.
The pin and upstream backlog are not changed by this implementation.

`socket-pktinfo` 0.4.1 supplies safe `recvmsg` / `WSARecvMsg` wrappers, including
the interface index. Its MSRV is 1.71, below this library's floor. The dependency
is optional under `std` and target-gated to Linux, macOS and Windows. A cloned
standard socket is created after binding and registered with Tokio; both handles refer to the same kernel
queue. Custom receives use `UdpSocket::try_io`, including WouldBlock readiness
clearing. The ordinary Tokio receive path drains disabled queues.
An explicit endpoint/receive regression covers the Winsock requirement not to
duplicate the unbound socket and assume a later bind updates the clone.

| Platform | Arrival identity | Forced response egress |
| --- | --- | --- |
| Linux | `IP_PKTINFO` / `ipi_ifindex` | `IP_UNICAST_IF`, plus the concrete local bind |
| macOS | `IP_PKTINFO` / `ipi_ifindex` | socket2's `IP_BOUND_IF` |
| Windows | `IP_PKTINFO` / `WSARecvMsg` | `IP_UNICAST_IF`, plus the concrete local bind |

Binding only a source IP is insufficient on weak-host systems. Linux/Windows
therefore have narrowly scoped `setsockopt` wrappers for `IP_UNICAST_IF`, in
network byte order. socket2 lacks that option; its Linux `SO_BINDTOIFINDEX`
alternative needs a newer kernel and also restricts reception, unlike
`IP_UNICAST_IF`. The evaluated Linux kernel permits the initial device binding
without `CAP_NET_RAW`, but requires that capability to change an existing device
binding. It is not a drop-in replacement for the existing local-delivery path
and does not add an incarnation token. `nix` has no typed
`IP_UNICAST_IF` option, and socket-pktinfo wraps reception, not egress options.
The socket remains owned during each synchronous call and each argument points
to an initialized, correctly sized integer. Packet decoding does not use local
unsafe code. Other hosted targets return `Unsupported` when constructing managed
discovery rather than silently reverting to prefix routing; their `no_std` core
is unaffected. ESP-IDF networking is not validated or supported by this path.

The Windows setter uses target-gated `windows-sys` 0.61.2 bindings (already
transitive through socket-pktinfo); the older winapi bindings do not expose
`IP_UNICAST_IF`. This dependency is optional under `std` as well.

Linux/Windows memberships use socket2's safe indexed membership methods.
Darwin requires `MCAST_JOIN_GROUP` / `MCAST_LEAVE_GROUP` with `group_req`:
its `IP_ADD_MEMBERSHIP` consumes only `ip_mreq`, ignoring the index appended by
socket2's `join_multicast_v4_n`. The real two-adapter fixture exposed this as an
incorrect first membership and `EADDRINUSE` on the second. Neither socket2 nor
nix provides the needed RFC 3678 wrapper, so it is a narrow local exception.
The request layout and option constants come from libc 0.2.191 rather than a
local `group_req` definition or hard-coded numbers. Its Rust 1.65 MSRV remains
below this library's Rust 1.87 floor. A native macOS test installs and removes an
indexed loopback membership, checking duplicate join and leave failures too;
this validates the actual syscall contract without modifying host interfaces.
Memberships are joined once per adapter even with multiple local aliases.
Outgoing multicast uses rustix's safe indexed `IP_MULTICAST_IF` wrapper on
Linux/macOS and Winsock's indexed `0.x.x.x` form through socket2 on Windows.
Address-only outgoing multicast setters are ambiguous when two adapters share
an address. The Unix setter no longer needs repository-owned unsafe code.

## Measurement reply source

Managed announcements advertise the concrete address **and port** of that
interface's registered discovery socket as the measurement endpoint. Its receive
worker also recognizes measurement PINGs and builds replies from the shared
session/clock state. Each PONG uses that same captured registration and the
existing checked send path; wildcard multicast reception does not serve PINGs.
This avoids wildcard source-address selection on overlapping subnets without
weakening exact source-endpoint matching in the measurement dispatcher. The
wildcard socket remains available for standalone measurement/responder usage.
Managed outbound PINGs use the captured discovery registration too. Hints are
keyed by peer identity and advertised endpoint, expire with the announcement TTL,
and are cleared on restart. Expired and invalidated leases are pruned on lookup,
recording, and reconciliation. Each sweep drains a shared topology monitor once
and compares all its leases against that snapshot and their cancellation flags.
The cache holds at most 1,024 hints overall and 16
endpoints per node; insertion at a limit evicts the earliest-expiring hint. Refresh
of an existing key does not evict another hint. A missing or stale hint fails closed. Each interface
registration owns a separate PONG dispatch table, so identical remote endpoints
on different adapters do not overwrite each other's routes. All PING retries use
the checked registered send path; this still does not close the atomic OS index
incarnation gap described below.
Measurement datagrams addressed to a unique bound port retain that registration's
ownership. Same-host delivery can name the loopback adapter; this is accepted
only when the registration's OS snapshot identifies both the loopback index and
an assigned local source address. Missing snapshot evidence fails closed. This
exception does not apply to discovery packets or foreign-adapter measurement
traffic, and does not relax exact remote endpoint matching for PONGs.

A bounded UDP regression demonstrates the problematic route choice first: a
wildcard sender selects a different address than the advertised bound socket.
It then decodes the real announcement, sends a PING to its measurement endpoint,
and verifies the PONG comes from that exact address and port. It changes no host
routes or interface configuration and does not establish atomic topology identity.
A separate loopback test enables the responder through `Messenger`, checks the
initial registration, removes and re-adds it through reconciliation, and verifies
both announcements and PONG sources for the new registration as well.

## Registration, queues and lifecycle

Each socket reader owns one delivery task and a 64-event discovery handoff queue.
ALIVE/RESPONSE decoding is bounded by the datagram size. Valid received traffic
refreshes only an already published route identity before attempting handoff;
overflow updates cannot add new hints or block measurement datagrams. Each item
retains its receive-time expiry, which also bounds the gateway's downstream wait.
Expired work is discarded, fractional remaining life uses the absolute deadline,
and older queued updates cannot overwrite a newer route refresh. The delivery task uses the lifecycle gate's receive
permit and preparation drain. Every queued item retains its captured ingress
lease; closure or lease invalidation interrupts its event-capacity wait. With
capacity reserved, the worker verifies the exact socket remains registered and
its generation is current, then records the route and publishes synchronously
under the interface-map lock. No route is recorded merely by queueing an update.
The worker is aborted when its owning reader exits. The bounded saturation regression
keeps the public discovery queue full while real UDP PING/PONG round trips finish,
then verifies stop/restart discards stale handoff events. A separate real-UDP
wildcard-reader regression fills downstream capacity, invalidates the old ingress
registration while its delivery waits, and verifies that only a replacement
registration's event and route survive when capacity returns. This local lease
proof does not establish atomic physical OS interface incarnation identity.

Measurement completion uses a private capacity-one terminal channel per job.
Success, timeout and send failure compete without blocking that private channel;
its winning result is forwarded to the public caller using cancellation-aware
backpressure. This distinguishes competing outcomes of one job from independent
public measurements sharing a result channel. Both full-private-channel and
shared-public-channel regressions cover the distinction.

Each registration retains index, name, local address and its own socket/Cancel
generation. Receiving a datagram and capturing its response registration are
serialized with interface-map changes. Response work retains that generation
across await points. Immediately before the nonblocking send, the map lock
protects the generation check and syscall together. Removal cancels parked
receivers and pending response/event forwarding; no lookup substitutes another
registration when the original has disappeared.

Before initial enumeration, discovery subscribes to raw topology notifications:
Linux route-netlink link/IPv4-address groups, macOS routing-socket interface and
address messages, and Windows IP Helper interface/address callbacks. Unix uses
safe nix sockets, with a narrowly scoped unsafe Tokio `AsyncFd::register` call.
The registration moves an open `OwnedFd` into `AsyncFd` and never replaces or
mutates that descriptor, so its identity remains stable until drop. Tokio
deprecated the safe constructors without a safe replacement; its socket wrappers
do not cover routing/netlink sockets, and `async-io` would add a second reactor.
Windows callback registration/cancellation is a separate local unsafe exception. `netwatcher` was evaluated but rejected: its drain followed by
snapshot-diff delivery collapses remove/add events whose final snapshot is
identical. This monitor conservatively advances a global generation for relevant
raw events, without comparing snapshots. Unrelated interface changes therefore
also rebuild discovery registrations.

Receive/send boundaries check the event stream and lease generation. The
scanner wakes on events; its five-second tick remains a retry mechanism, not the
only source of lifetime changes. Enumeration is bracketed by generation checks,
and registration refuses a stale snapshot. Stream errors invalidate leases and
are logged, rather than allowing stale sends. Windows callback cancellation
waits for callbacks before freeing their context; a failed native cancellation
is logged and retains that context instead of risking use-after-free.

The public `InterfaceSockets` alias remains address-keyed. Each value privately
groups the distinct adapters sharing that address; all managed receive, send,
broadcast and reconciliation paths visit the complete group. Captured leases
omit sibling entries, so removing one adapter does not retain another adapter's
receiver through unrelated pending work. The map's length is an address count,
not an adapter count.

Successful empty scans remove all interfaces. Changed index or name with the
same address creates a new generation. Removal and new membership publication
drain the wildcard queue while holding the registration lock, preventing old
queued packets from acquiring a replacement registration at those boundaries.
Scan failures are logged and distinguished from successful empty scans.
If no identified interface is available, discovery waits for the next scan;
there is no `0.0.0.0` default-interface fallback.

The existing acknowledged startup gates, disabled draining, receive rearming,
epoch cancellation, scoped JoinSets and final cleanup remain in use. Adding a
receiver still registers with the startup barrier before spawning it. Managed
broadcast sends also validate their snapshot's socket generation. Standalone
public send helpers still send through the socket explicitly supplied by their
caller; this change does not invent adapter metadata for arbitrary public sockets.

## Evidence and remaining limits

Normal serial tests include a deterministic overlapping-prefix negative control
(the former algorithm selects the wrong adapter), missing/zero ingress rejection,
replacement-generation rejection, successful-empty-scan teardown, disabled
receiver cleanup, real loopback packet-info reception/response source checks,
and the pre-existing disable/enable regressions.

The Linux CI test job additionally runs
`.github/scripts/test-discovery-ingress.sh` in private mount/network namespaces.
Two peer namespaces reach two different veth adapters on the same `/24`; each
sender's address is numerically closer to the wrong local adapter. It checks
real multicast ingress and response source on both links across three restarts,
deletes an adapter before reconciliation to test kernel egress rejection,
recreates it, and rejects old-generation work. These tests are ignored outside
the explicitly configured fixture, not silently passed on unsuitable hosts.

The namespace fixture additionally deletes/recreates an adapter with its old
index, name and address without reconciling in between, then assigns one local
address to both adapters and verifies both peer namespaces receive responses.
Linux and Darwin additionally capture a managed outgoing multicast announcement
from each duplicate-address registration on its opposite peer link. A readiness
handshake precedes each send; both the source address and the registration's
unique port must match. Unicast response coverage alone cannot validate
`IP_MULTICAST_IF`, so these are separate assertions.

The macOS and Windows fixture scripts configure distinct adapters on a
disposable CI runner. macOS sends across paired `feth` ports with explicit
interface-scoped routes for the overlapping links. A CI-only pinned Scapy/BPF
helper supplies ARP and UDP peers without assigning their addresses to the host
kernel, avoiding same-host local routing altogether. Windows uses real multicast
loop delivery on private virtual-switch adapters, not the loopback pseudo-interface.
They verify the response registration's unique endpoint and the receiving peer
adapter index across restarts, then remove/re-add an address with an unchanged
final identity. macOS also exercises duplicate-address groups.
Windows provisions private Hyper-V/HNS adapters and adds private aliases without
disabling DHCP on the runner's existing transport interface.
The Darwin helper captures on the opposite Ethernet port and verifies the
sender's unique registered endpoint. Windows also asserts the reply's packet-info
index matches its peer adapter. This is virtual-link evidence, not an external
physical-network test. Scapy is only a Python CI-fixture dependency; library
dependencies and wire encoding do not change.
The Windows runner rejected the second duplicate IPv4 assignment with native
error 5010 (`ERROR_OBJECT_ALREADY_EXISTS`). That exact error is reported as
missing duplicate-address network evidence while overlap/churn assertions still
run; other setup errors fail. No Windows duplicate-network pass is claimed.
Both fixtures fail if their required overlap/churn assertions fail.
The fixture also tries the independent NetTCPIP `New-NetIPAddress` provider
when the native IP Helper call returns 5010. This is confined to the adapters
it created, never the runner's transport adapter. Provider success must pass
the existing duplicate-address unicast-response assertions. Managed outgoing
multicast capture on macOS observes the opposite peer port. The Windows fixture
now also sends an ALIVE from each duplicate-address registration and checks
Pktmon NIC-only Tx records on both fixture adapters. Those records must identify
the distinct registered source endpoints and the Link multicast destination.
The fixture consumes the test's endpoint/interface markers rather than accepting
an arbitrary source port at the duplicate address. Its isolated parser tests
cover matching, invalid endpoint markers, no-UDP captures, wrong-port,
cross-adapter, additional wrong-NIC transmissions,
ambiguous/unmapped NIC identities, stale headers and unsupported-format captures; the
Windows CI job runs them before provisioning adapters. Unsupported capture
tooling is reported as missing evidence, separately from a valid capture that
fails to show a required transmission. Missing prerequisites skip only the
duplicate-capture evidence: the existing overlap/restart/churn assertions must
still pass, and a green Windows job is not a duplicate-network pass. A valid
capture missing a required endpoint's NIC Tx, or also transmitting that endpoint
on the wrong fixture NIC, fails the fixture. Recognized packet metadata with no
UDP records reports both transmissions as missing; a changed packet decoder
cannot turn that absence into a tooling skip. Missing or invalid endpoint markers
fail after every successful duplicate-enabled test run, even without capture
tooling, outside the capture-error fallback.
This is local virtual-NIC transmission evidence, not independent peer receipt:
provider success plus these records would still not prove the full Windows
criterion. A matching 5010 remains missing evidence and is explicitly reported
as skipped in the job summary; any other failure fails setup. This tests a second provisioning
API, not a different Windows network stack or an independent physical topology.
These same-host tests are distinct from Linux's independent peer namespaces;
they do not establish behavior across external physical networks.

Platform fixture results must be checked on the current PR head before claiming
closure of #154. Neither a compilation nor a deterministic selector test is a
real multihomed network test. OS notifications and socket syscalls are separate
operations: packet metadata still contains an index, not an atomic OS generation
token. This implementation rejects leases after observed raw changes, including
equal-snapshot changes; it cannot claim atomic exclusion of index reuse during
the notification-delivery/check/send race itself. Other hosted targets remain
unsupported for managed discovery, as described above.

On September 9, 2026, CI run `34372516043` passed all three platform jobs:
Linux's isolated namespaces included forced identical-index replacement and
duplicate addresses; Darwin's raw Ethernet peers completed all nine exchanges,
including both duplicate-address adapters; Windows completed overlap/restart/churn
and explicitly reported its duplicate-address setup rejection. The native serial
suite passed 347 library and 33 integration tests (Windows has one additional
SDK-layout assertion). The equal-snapshot regression was also run as a negative
control: forcing the topology generation to remain zero made its stale-send
assertion fail; restoring generation checks made it pass.

These results close the polling and address-keyed-storage gaps, but **do not
claim full closure of #154**: Windows duplicate-network behavior and atomic
notification/check/send exclusion remain unproven. The Windows configuration
rejection is not counted as a successful duplicate-address network test.

### Remaining lifetime and safe-wrapper blockers

The September 10 investigation checked kernel storage, not only option names.
Linux `do_ip_setsockopt(IP_UNICAST_IF)` looks up the device, releases the device
reference with `dev_put`, then stores the integer in `inet->uc_index`.
`IP_MULTICAST_IF` similarly stores `mc_index`. Microsoft specifies
`IP_UNICAST_IF` as a network-order `IF_INDEX`, not a LUID or generation-bearing
handle. Retaining a userspace socket or registration Arc therefore does not
establish the required kernel identity lifetime on those paths.

Darwin's `inp_bindif` resolves an interface pointer and `inp_bindif_common`
stores `inp_boundifp`, but output subsequently derives
`ifscope = inp->inp_boundifp->if_index`. Pointer storage alone therefore does not
establish a receive-to-egress incarnation contract. More user-space checks, or
replacing the listener while retaining the same indexed send options, would not
prove atomic exclusion.

Complete repository-owned unsafe removal is also unfinished. `rustix` replaced
Unix multicast egress and `crossterm` replaced the Windows example console calls.
The remaining unicast/membership options still lack suitable safe APIs in the
evaluated socket2/nix/rustix interfaces. Windows notification replacement must
preserve raw changes even when the final address set is identical. Unix topology
registration also retains the `AsyncFd::register` exception and ownership proof
described above; a safe replacement must preserve its reactor and descriptor
lifetime guarantees.
`if-watch` 3.2.2 was independently evaluated: its Windows callback sets a resync
flag, then its public stream emits an address-set difference, so it loses that
case just as the rejected snapshot watcher does. No local FFI was moved into a
shim dependency or replaced by an unsupported-platform success path.

### November 9, 2026 implementation decision

The issue's September body describes the original routing defect; its September
10 and November 5 comments describe the stricter remaining acceptance criteria.
The original routing fix is already implemented. This follow-up does not reopen
the upstream backlog or replace packet reception.

| Remaining criterion | Supported implementation/evidence | Still required for closure |
| --- | --- | --- |
| Receive-to-egress incarnation identity | Captured ingress index, registered socket, observed raw-event invalidation and checked nonblocking send | A documented kernel lifetime primitive and replacement at the final send boundary, for both unicast and managed multicast |
| Windows duplicate-address networks | Existing overlap/restart/churn fixture; provisioning rejection remains missing evidence | Usable duplicate-address adapters and independent peer-side observation of both response endpoints and multicast egress, or a configuration-specific authoritative limit |
| Safe platform integration | Safe packet reception and Unix multicast egress; native libc Darwin membership ABI | Suitable safe unicast/membership, raw Windows notification and Unix reactor-registration APIs without losing raw equal-final-state events or owned shutdown |

Established implementations such as Avahi and systemd-resolved pass the interface
index using `IP_PKTINFO` ancillary data. This is a useful precedent for
index-scoped routing, not a generation-bearing lifetime primitive. Passing the
same index per send rather than setting a socket option cannot, by itself,
distinguish a replacement adapter that has acquired that index.
Likewise, per-interface multicast listeners, `SO_BINDTOIFINDEX`, additional
generation checks, and converting Windows indices to LUIDs in userspace do not
establish atomic identity at the UDP send boundary.

The safe-wrapper inventory was rechecked against socket2 0.6.5, rustix 1.1.6,
nix 0.31.3 and Tokio 1.54.1. The native Darwin request binding reduces local ABI
maintenance; it does not turn the remaining `setsockopt` call into a safe crate
API. The existing reactor-registration and Windows raw-notification exceptions
remain, rather than adding a second reactor or losing equal-final-state events.

No suitable lifetime primitive was established in this investigation. This is
not proof that no such primitive can exist. A privileged namespace/service
redesign or a kernel change would be a different deployment contract, not an
unconditional fix for the current unprivileged portable library. The retained
contract is index-scoped delivery plus rejection of **observed** topology
changes. No issue closure is claimed from a source review, native membership
test or unexecuted platform fixture.

## Primary implementation references

* Upstream: `vendor/ableton-link/include/ableton/discovery/IpInterface.hpp`,
  `UdpMessenger.hpp`, and `platforms/asio/Context.hpp` at the unchanged pin.
* Packet metadata: <https://github.com/pixsper/socket-pktinfo/tree/v0.4.1/src>
* Linux options/routing: <https://github.com/torvalds/linux/blob/master/net/ipv4/ip_sockglue.c>
  and <https://github.com/torvalds/linux/blob/master/net/ipv4/udp.c>
* November 9 pinned Linux checks:
  [`IP_UNICAST_IF` stores an index](https://github.com/torvalds/linux/blob/7475ea8b58616b33c04afb976cf0fbaad9dd63fb/net/ipv4/ip_sockglue.c#L1127-L1156),
  [bound-device option and capability check](https://github.com/torvalds/linux/blob/7475ea8b58616b33c04afb976cf0fbaad9dd63fb/net/core/sock.c#L636-L662),
  [unicast/multicast output-index selection](https://github.com/torvalds/linux/blob/7475ea8b58616b33c04afb976cf0fbaad9dd63fb/net/ipv4/udp.c#L1393-L1412),
  and [route lookup by index](https://github.com/torvalds/linux/blob/7475ea8b58616b33c04afb976cf0fbaad9dd63fb/net/ipv4/route.c#L2814-L2817).
* Windows option contract:
  <https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ip-socket-options>
* socket2 adapter binding:
  <https://docs.rs/socket2/0.6.5/socket2/struct.Socket.html#method.bind_device_by_index_v4>
* Darwin indexed multicast:
  <https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/in_mcast.c>
* Darwin bound-interface storage:
  <https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/in_pcb.c>
  and [pinned conversion back to an output scope index](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/netinet/in_pcb.c#L1593-L1596).
* Native Darwin RFC 3678 bindings:
  <https://github.com/rust-lang/libc/blob/0.2.191/src/unix/bsd/apple/mod.rs>.
* Comparable indexed send/receive designs:
  [Avahi](https://github.com/avahi/avahi/blob/7d8af13d62f12ee046c3bdc95c1b2676c054d2d1/avahi-core/socket.c#L544-L562)
  and [systemd-resolved](https://github.com/systemd/systemd/blob/6cf990ea2931a901aaba4e0e5ee1d8fda86a4a76/src/resolve/resolved-manager.c#L1176).
* Windows NIC capture:
  <https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/pktmon-start>.
* Evaluated Windows watcher:
  <https://docs.rs/crate/if-watch/3.2.2/source/src/win.rs>
* Windows notifications and cancellation:
  <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-notifyipinterfacechange>
  and <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-cancelmibchangenotify2>

## Source migration

`PeerStateMessageType` adds `expires_at: Option<tokio::time::Instant>` and
`observation: Option<Arc<PeerObservation>>`. Standalone literals use None for both;
managed forwarders retain both fields and the wire TTL. `PeerEvent::Observed` must
be forwarded intact to `GatewayObserver`; managed BYEBYE is a zero-TTL peer-state
withdrawal. Session construction uses `Session::new`; requests/results and standalone
measurement/result helpers use the originating `SessionKey` (`session.key()`). The
new `MeasurePeerEvent::Cancelled(key, attempted_peer)` denotes retryable registration retirement.
See the [source migration guide](migration.md)
for every affected public signature and enum, session-aware observers, retry ownership,
cooperative contention and clock publication. These source-breaking changes do not
change the wire format. Same-adapter alias selection uses
verified subnet masks and link-local scope only after the ingress index is fixed.
Periodic local-delivery identity is collected with interface enumeration in the blocking
scan and reused for all registrations in that reconciliation, including leases
captured before a failed identity snapshot recovered. This does not close #154.

Receive ordering, node supersession and committed departure cleanup are documented
in the [managed discovery overview](migration.md#managed-discovery-overview).
