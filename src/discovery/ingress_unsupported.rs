use std::{
    io,
    net::{IpAddr, SocketAddr, SocketAddrV4},
    ops::Deref,
    sync::Arc,
};
use tokio::net::UdpSocket;

pub(super) struct PacketInfo {
    pub if_index: u64,
    pub addr_src: SocketAddr,
    pub addr_dst: IpAddr,
}

pub(super) struct PacketSocket {
    pub socket: Arc<UdpSocket>,
}

impl Deref for PacketSocket {
    type Target = UdpSocket;
    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}

impl PacketSocket {
    pub fn new(_addr: SocketAddrV4, _index: Option<u32>) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ingress-aware discovery is implemented on Linux, macOS and Windows",
        ))
    }

    pub fn try_recv(&self, _buf: &mut [u8]) -> io::Result<(usize, PacketInfo)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "arrival metadata is unavailable",
        ))
    }
}
