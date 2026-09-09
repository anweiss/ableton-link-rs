"""A raw Ethernet peer on the other end of a disposable Darwin feth pair.

Its IPv4 address is deliberately NOT assigned to the host kernel. Replies must
cross the virtual link instead of taking ambiguous same-host local routes.
"""
import argparse
import os
import sys
import threading

from scapy.all import ARP, Ether, IP, UDP, AsyncSniffer, conf, get_if_hwaddr, sendp


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--interface", required=True, choices=["feth1541", "feth1543"])
    parser.add_argument("--local", required=True)
    parser.add_argument("--group", required=True)
    parser.add_argument("--expected", required=True)
    parser.add_argument("--payload", required=True)
    parser.add_argument("--broadcast", action="store_true")
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.geteuid() != 0:
        raise RuntimeError("requires the disposable, privileged CI fixture")
    conf.use_bpf = True
    conf.sniff_promisc = False
    mac = get_if_hwaddr(args.interface)
    expected_ip, expected_port = args.expected.rsplit(":", 1)
    ready = threading.Event()
    received = threading.Event()
    replies = []
    group = [int(octet) for octet in args.group.split(".")]
    multicast_mac = "01:00:5e:%02x:%02x:%02x" % (group[1] & 0x7F, group[2], group[3])

    def handle(packet):
        if ARP in packet and packet[ARP].op == 1 and packet[ARP].pdst == args.local:
            arp = packet[ARP]
            reply = Ether(src=mac, dst=arp.hwsrc) / ARP(
                op=2, hwsrc=mac, psrc=args.local, hwdst=arp.hwsrc, pdst=arp.psrc
            )
            sendp(reply, iface=args.interface, verbose=False)
        destination = args.group if args.broadcast else args.local
        port = 20808 if args.broadcast else 20809
        if IP in packet and UDP in packet and packet[IP].dst == destination and packet[UDP].dport == port:
            replies.append(packet)
            received.set()

    sniffer = AsyncSniffer(
        iface=args.interface,
        filter="arp or udp",
        store=False,
        prn=handle,
        started_callback=ready.set,
        promisc=False,
    )
    sniffer.start()
    try:
        if not ready.wait(5):
            raise RuntimeError("BPF peer capture did not become ready")
        announcement = (
            Ether(src=mac, dst=multicast_mac)
            / IP(src=args.local, dst=args.group, ttl=2)
            / UDP(sport=20809, dport=20808)
            / bytes.fromhex(args.payload)
        )
        if args.broadcast:
            print("LINK154_READY", flush=True)
        else:
            sendp(announcement, iface=args.interface, verbose=False)
        if not received.wait(5):
            raise RuntimeError("no discovery response reached raw peer " + args.interface)
        reply = replies[0]
        actual = (reply[IP].src, reply[UDP].sport)
        if actual != (expected_ip, int(expected_port)):
            raise AssertionError(f"wrong ingress response socket: {actual}, expected {args.expected}")
        print(f"Ethernet response on {args.interface} from {actual}", file=sys.stderr)
        sys.stdout.buffer.write(bytes(reply[UDP].payload))
    finally:
        sniffer.stop()


if __name__ == "__main__":
    main()
