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
alternative needs privileges and a newer kernel. `nix` has no typed
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
Memberships are joined once per adapter even with multiple local aliases.
Outgoing multicast uses `IP_MULTICAST_IF` with `ip_mreqn` on Linux/macOS and
Winsock's indexed `0.x.x.x` form on Windows. The Unix setter is another narrow
unsafe exception: socket2 and nix expose address-only outgoing multicast
setters, which are ambiguous when two adapters share an address.

## Registration, queues and lifecycle

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
safe nix sockets and Tokio `AsyncFd`; only the Windows callback ownership needs
local unsafe code. `netwatcher` was evaluated but rejected: its drain followed by
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

The macOS and Windows fixture scripts configure two distinct adapters on a
disposable CI runner. They use real multicast loop delivery on those adapters
(not `lo0` / the loopback pseudo-interface), verify the selected registration's
response source address and unique port across restarts, remove/re-add an address with an unchanged final identity,
and exercise duplicate-address groups. macOS uses temporary `feth` pairs;
Windows provisions private Hyper-V/HNS adapters and adds private aliases without
disabling DHCP on the runner's existing transport interface.
Same-host unicast replies can take local delivery through `lo0`; the Darwin
client does not apply `IP_BOUND_IF` to receipt. Outgoing multicast remains pinned.
The endpoint assertion proves which registered socket responded, not
external-wire unicast traversal.
The Windows runner rejected the second duplicate IPv4 assignment with native
error 5010 (`ERROR_OBJECT_ALREADY_EXISTS`). That exact error is reported as
missing duplicate-address network evidence while overlap/churn assertions still
run; other setup errors fail. No Windows duplicate-network pass is claimed.
Both fixtures fail if their required overlap/churn assertions fail.
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

## Primary implementation references

* Upstream: `vendor/ableton-link/include/ableton/discovery/IpInterface.hpp`,
  `UdpMessenger.hpp`, and `platforms/asio/Context.hpp` at the unchanged pin.
* Packet metadata: <https://github.com/pixsper/socket-pktinfo/tree/v0.4.1/src>
* Linux options/routing: <https://github.com/torvalds/linux/blob/master/net/ipv4/ip_sockglue.c>
  and <https://github.com/torvalds/linux/blob/master/net/ipv4/udp.c>
* Windows option contract:
  <https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ip-socket-options>
* socket2 adapter binding:
  <https://docs.rs/socket2/0.6.5/socket2/struct.Socket.html#method.bind_device_by_index_v4>
* Darwin indexed multicast:
  <https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/in_mcast.c>
* Windows notifications and cancellation:
  <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-notifyipinterfacechange>
  and <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-cancelmibchangenotify2>
