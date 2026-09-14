#![allow(clippy::too_many_arguments)]

pub mod gateway;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod ingress;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[path = "ingress_unsupported.rs"]
mod ingress;
pub mod interface_scanner;
pub mod ip_interface;
pub mod messages;
pub mod messenger;
pub mod multi_interface_messenger;
pub mod peers;
mod topology;

use std::net::{Ipv4Addr, SocketAddrV4};

pub const LINK_PORT: u16 = 20808;
pub const MULTICAST_IP_ANY: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), LINK_PORT);
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 76, 78, 75);

#[cfg(test)]
mod socket_test_support {
    use std::{io::ErrorKind, net::SocketAddr};

    use socket2::{Domain, SockRef, Socket, Type};
    use tokio::net::UdpSocket;

    pub(super) fn assert_ephemeral_socket_is_unshared(socket: &UdpSocket) {
        let options = SockRef::from(socket);
        assert!(!options.reuse_address().unwrap());
        #[cfg(unix)]
        assert!(!options.reuse_port().unwrap());

        let endpoint = socket.local_addr().unwrap();
        assert_ne!(endpoint.port(), 0);
        let intruder = Socket::new(Domain::for_address(endpoint), Type::DGRAM, None).unwrap();
        // Winsock permits SO_REUSEADDR to take over a non-exclusive endpoint.
        // Test an ordinary bind there; option readback above detects regressions.
        #[cfg(unix)]
        {
            intruder.set_reuse_address(true).unwrap();
            intruder.set_reuse_port(true).unwrap();
        }
        let error = intruder
            .bind(&endpoint.into())
            .expect_err("another socket bound the ephemeral endpoint");
        assert_eq!(error.kind(), ErrorKind::AddrInUse);
    }

    pub(super) fn shared_loopback_socket(addr: SocketAddr) -> Socket {
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, None).unwrap();
        socket.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        socket.set_reuse_port(true).unwrap();
        // Keep the reservation open so another process cannot claim the port
        // between selecting it and testing a constructor's nonzero-port path.
        socket.bind(&addr.into()).unwrap();
        socket
    }
}
