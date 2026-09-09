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

## Registration, queues and lifecycle

Each registration retains index, name, local address and its own socket/Cancel
generation. Receiving a datagram and capturing its response registration are
serialized with interface-map changes. Response work retains that generation
across await points. Immediately before the nonblocking send, the map lock
protects the generation check and syscall together. Removal cancels parked
receivers and pending response/event forwarding; no lookup substitutes another
registration when the original has disappeared.

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

This is **not complete closure of #154**:

* Interface scanning is periodic (five seconds), not an OS change-event stream.
  Removal/recreation that reuses the same index, name and address entirely
  between scans cannot be distinguished. Packet-info has no registration
  generation; data queued before an unobserved replacement can be misattributed.
* Distinct local addresses on overlapping prefixes are supported. The public
  interface map is keyed by local IPv4 address; duplicate addresses on different
  adapters are logged and excluded rather than arbitrarily choosing one.
* Real two-adapter multicast/removal behavior on macOS and Windows has not been
  exercised by the Linux fixture. Their normal tests exercise OS packet metadata
  on loopback and lifecycle handling; compilation is not proof of multihomed
  behavior.
* Multicast membership APIs here still select adapters by local address. Address
  migration during setup is subject to the same polling limitation. Egress setup
  failures are logged and that registration is not published.

Keep #154 open until those acceptance gaps have explicit coverage or an agreed
scope. Do not label a platform compile or the deterministic selector tests as a
real multihomed network test.

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
