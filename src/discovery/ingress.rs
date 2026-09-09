use std::{io, net::SocketAddrV4, ops::Deref, sync::Arc};

use socket_pktinfo::{PktInfo, PktInfoUdpSocket};
use tokio::{io::Interest, net::UdpSocket};

/// The Tokio socket and packet-info handle share one kernel receive queue.
/// All reads go through Tokio's readiness accounting, including disabled drains.
pub(super) struct PacketSocket {
    pub socket: Arc<UdpSocket>,
    info: PktInfoUdpSocket,
}

impl Deref for PacketSocket {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl PacketSocket {
    pub fn new(addr: SocketAddrV4, index: Option<u32>) -> io::Result<Self> {
        let info = PktInfoUdpSocket::new(socket2::Domain::IPV4)?;
        info.set_nonblocking(true)?;
        info.set_reuse_address(true)?;
        #[cfg(unix)]
        info.set_reuse_port(true)?;
        // Winsock's duplicated descriptor must be created after bind; a clone
        // of the unbound descriptor does not acquire the later local endpoint.
        info.bind(&addr.into())?;
        let std_socket = info.try_clone_std()?;
        std_socket.set_nonblocking(true)?;
        let options = socket2::SockRef::from(&std_socket);
        #[cfg(target_os = "linux")]
        options.set_multicast_all_v4(false)?;
        options.set_multicast_loop_v4(true)?;
        options.set_multicast_ttl_v4(2)?;
        options.set_broadcast(true)?;
        if let Some(index) = index {
            pin_egress(&std_socket, index)?;
            options.set_multicast_if_v4(addr.ip())?;
        }
        Ok(Self {
            socket: Arc::new(UdpSocket::from_std(std_socket)?),
            info,
        })
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<(usize, PktInfo)> {
        self.socket
            .try_io(Interest::READABLE, || self.info.recv(buf))
    }
}

#[cfg(target_os = "macos")]
fn pin_egress(socket: &std::net::UdpSocket, index: u32) -> io::Result<()> {
    let index = std::num::NonZeroU32::new(index)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero interface index"))?;
    socket2::SockRef::from(socket).bind_device_by_index_v4(Some(index))
}

// socket2 exposes IP_BOUND_IF on macOS but no IP_UNICAST_IF on Linux/Windows.
// Its Linux SO_BINDTOIFINDEX alternative needs CAP_NET_RAW and a recent kernel.
// socket-pktinfo only wraps reception; nix has no typed IP_UNICAST_IF option.
// Keep the two missing setsockopt wrappers here, not a custom recvmsg/WSARecvMsg.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn pin_egress(socket: &std::net::UdpSocket, index: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero interface index",
        ));
    }
    let value = index.to_be();
    // SAFETY: a live socket, and a correctly sized initialized u32 borrowed
    // only for this synchronous call. IP_UNICAST_IF takes network byte order.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_UNICAST_IF,
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn pin_egress(socket: &std::net::UdpSocket, index: u32) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{self, IPPROTO_IP, IP_UNICAST_IF};
    if index == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero interface index",
        ));
    }
    let value = index.to_be();
    // SAFETY: same synchronous, initialized u32 option contract as on Linux;
    // no pointer escapes and the socket remains owned by the caller.
    let result = unsafe {
        WinSock::setsockopt(
            socket.as_raw_socket() as WinSock::SOCKET,
            IPPROTO_IP,
            IP_UNICAST_IF,
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of_val(&value) as i32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        // SAFETY: reads this thread's Winsock error immediately after failure.
        Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn packet_info_and_tokio_handles_share_the_bound_endpoint() {
        let socket = PacketSocket::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), None).unwrap();
        let packet_info_endpoint = socket.info.try_clone_std().unwrap().local_addr().unwrap();
        assert_ne!(packet_info_endpoint.port(), 0);
        assert_eq!(socket.local_addr().unwrap(), packet_info_endpoint);
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        sender
            .send_to(b"metadata", packet_info_endpoint)
            .await
            .unwrap();
        let mut buffer = [0; 32];
        let (size, metadata) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                socket.readable().await.unwrap();
                match socket.try_recv(&mut buffer) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    result => break result.unwrap(),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(&buffer[..size], b"metadata");
        assert_eq!(metadata.addr_src, sender.local_addr().unwrap());
        assert_ne!(metadata.if_index, 0);
    }
}
