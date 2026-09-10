#!/usr/bin/env python3
"""Falsify driverless egress candidates in the disposable Linux fixture.

This is a research probe, not a passing closure test for #154. It intentionally
expects the current indexed UDP mechanism to send through a replacement.
"""

import contextlib
import json
import os
from pathlib import Path
import selectors
import socket
import struct
import subprocess
import sys

HOST = "10.42.0.1"
PEER = "10.42.0.130"
PORT = 20809
ETH_IP = 0x0800


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs)


def line(process):
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        if not selector.select(10):
            raise TimeoutError("fixture peer did not acknowledge within 10 seconds")
    value = process.stdout.readline()
    if not value:
        raise RuntimeError(f"fixture peer exited: {process.poll()}")
    return value.rstrip("\n")


@contextlib.contextmanager
def peer():
    process = subprocess.Popen(
        ["ip", "netns", "exec", "link154-a", sys.executable, __file__, "--peer"],
        stdout=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    try:
        assert line(process) == "READY"
        yield process
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def packet_socket():
    result = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETH_IP))
    try:
        result.bind(("veth-a", 0))
    except BaseException:
        result.close()
        raise
    return result


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def frame(source_port, payload):
    host = bytes.fromhex(Path("/sys/class/net/veth-a/address").read_text().strip().replace(":", ""))
    remote = json.loads(run("ip", "-n", "link154-a", "-j", "link", "show", "peer-a").stdout)[0]
    destination = bytes.fromhex(remote["address"].replace(":", ""))
    udp = struct.pack("!HHHH", source_port, PORT, 8 + len(payload), 0) + payload
    header = struct.pack(
        "!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 0, 0, 2,
        socket.IPPROTO_UDP, 0, socket.inet_aton(HOST), socket.inet_aton(PEER),
    )
    header = header[:10] + struct.pack("!H", checksum(header)) + header[12:]
    return destination + host + struct.pack("!H", ETH_IP) + header + udp


def expect(process, payload):
    received = json.loads(line(process))
    assert received["payload"] == payload.decode(), received
    assert received["source"][0] == HOST, received


def main():
    if sys.platform != "linux" or os.environ.get("LINK_154_NETNS") != "1":
        raise RuntimeError("requires the explicitly configured disposable Linux fixture")
    for namespace in ("net", "mnt"):
        if os.readlink(f"/proc/self/ns/{namespace}") == os.readlink(f"/proc/1/ns/{namespace}"):
            raise RuntimeError(f"refusing to alter the host {namespace} namespace")
    if sys.argv[1:] == ["--peer"]:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
            receiver.bind((PEER, PORT))
            print("READY", flush=True)
            while True:
                data, source = receiver.recvfrom(2048)
                print(json.dumps({"payload": data.decode(), "source": source}), flush=True)
        return
    if sys.argv[1:]:
        raise ValueError("unknown probe arguments")
    index = socket.if_nametoindex("veth-a")
    with contextlib.ExitStack() as stack:
        udp = stack.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
        udp.bind((HOST, 0))
        udp.setsockopt(socket.IPPROTO_IP, 50, struct.pack("!I", index))  # IP_UNICAST_IF
        bound_udp = stack.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
        bound_udp.bind((HOST, 0))
        bound_udp.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, b"veth-a\0")
        packet = stack.enter_context(packet_socket())
        with peer() as receiver:
            udp.sendto(b"original-indexed-udp", (PEER, PORT))
            expect(receiver, b"original-indexed-udp")
            bound_udp.sendto(b"original-bound-device-udp", (PEER, PORT))
            expect(receiver, b"original-bound-device-udp")
            packet.send(frame(udp.getsockname()[1], b"original-bound-packet"))
            expect(receiver, b"original-bound-packet")

        # The identity check succeeds here. Replacement happens after it, with
        # no further userspace identity/notification check before the sends.
        assert socket.if_nametoindex("veth-a") == index
        run(
            "bash", ".github/scripts/test-discovery-ingress.sh", "--reuse-a",
            env={**os.environ, "LINK_154_INDEX": str(index)},
        )
        assert socket.if_nametoindex("veth-a") == index
        with peer() as receiver:
            udp.sendto(b"stale-indexed-udp", (PEER, PORT))
            expect(receiver, b"stale-indexed-udp")
            bound_udp.sendto(b"stale-bound-device-udp", (PEER, PORT))
            expect(receiver, b"stale-bound-device-udp")
            try:
                packet.send(frame(udp.getsockname()[1], b"stale-bound-packet"))
            except OSError as error:
                if error.errno != 6:  # ENXIO: cached device was cleared on unregister.
                    raise
                packet_error = {"errno": error.errno, "message": str(error)}
            else:
                raise AssertionError("old AF_PACKET handle accepted a send after replacement")
            with packet_socket() as fresh:
                fresh.send(frame(udp.getsockname()[1], b"replacement-bound-packet"))
                expect(receiver, b"replacement-bound-packet")
        print(json.dumps({
            "platform": sys.platform,
            "kernel": os.uname().release,
            "reused_index": index,
            "indexed_udp": "stale packet received on replacement",
            "bind_to_device_udp": "stale packet received on replacement",
            "bound_packet": packet_error,
            "fresh_bound_packet": "received on replacement",
            "closure": False,
        }, sort_keys=True))


if __name__ == "__main__":
    main()
