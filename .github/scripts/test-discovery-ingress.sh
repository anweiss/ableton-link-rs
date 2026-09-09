#!/usr/bin/env bash
set -euo pipefail

# Refuse to mutate a host namespace, including the replacement helper.
[[ "$(readlink /proc/self/ns/net)" != "$(readlink /proc/1/ns/net)" ]]
[[ "$(readlink /proc/self/ns/mnt)" != "$(readlink /proc/1/ns/mnt)" ]]

create_a() {
  ip link add veth-a type veth peer name peer-a
  ip link set peer-a netns link154-a
  ip addr add 10.42.0.1/24 dev veth-a
  ip link set veth-a up
  ip netns exec link154-a ip addr add 10.42.0.130/24 dev peer-a
  ip netns exec link154-a ip link set peer-a up
  ip netns exec link154-a ip route add 224.0.0.0/4 dev peer-a
}

if [[ "${1:-}" == "--replace-a" ]]; then
  [[ "${LINK_154_NETNS:-}" == "1" ]]
  create_a
  exit
fi

# Refuse to mutate a host namespace: invoke with sudo unshare --mount --net.
test -x "$1"
mount --make-rprivate /
mkdir -p /run/netns
mount -t tmpfs tmpfs /run/netns
export LINK_154_NETNS=1
ip link set lo up
ip netns add link154-a
ip netns add link154-b
trap 'ip netns del link154-a; ip netns del link154-b' EXIT
ip netns exec link154-a ip link set lo up
ip netns exec link154-b ip link set lo up
create_a
ip link add veth-b type veth peer name peer-b
ip link set peer-b netns link154-b
ip addr add 10.42.0.129/24 dev veth-b
ip link set veth-b up
ip netns exec link154-b ip addr add 10.42.0.2/24 dev peer-b
ip netns exec link154-b ip link set peer-b up
ip netns exec link154-b ip route add 224.0.0.0/4 dev peer-b
# Overlapping connected routes intentionally defeat source-prefix inference.
# Disable reverse-path filtering only inside these temporary namespaces.
sysctl -qw net.ipv4.conf.all.rp_filter=0 net.ipv4.conf.default.rp_filter=0
sysctl -qw net.ipv4.conf.veth-a.rp_filter=0 net.ipv4.conf.veth-b.rp_filter=0
"$1" --ignored --exact discovery::messenger::tests::multihomed_namespace_ingress_and_churn --nocapture
