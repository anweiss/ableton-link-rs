use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// IPv6 multicast address for Ableton Link
/// This is a non-permanently-assigned link-local multicast address (RFC4291)
pub const MULTICAST_ADDR_V6: Ipv6Addr = Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0, 0x8080);

/// Get the multicast endpoint for IPv4
pub fn multicast_endpoint_v4() -> SocketAddrV4 {
    SocketAddrV4::new(crate::discovery::MULTICAST_ADDR, crate::discovery::LINK_PORT)
}

/// Get the multicast endpoint for IPv6 with the given scope ID
pub fn multicast_endpoint_v6(scope_id: u32) -> SocketAddrV6 {
    let mut addr = SocketAddrV6::new(MULTICAST_ADDR_V6, crate::discovery::LINK_PORT, 0, scope_id);
    addr.set_scope_id(scope_id);
    addr
}

/// Get the appropriate multicast endpoint for the given IP address
pub fn multicast_endpoint_for_addr(addr: &IpAddr) -> SocketAddr {
    match addr {
        IpAddr::V4(_) => SocketAddr::V4(multicast_endpoint_v4()),
        IpAddr::V6(ipv6) => {
            let scope_id = if ipv6.is_unicast_link_local() {
                // For link-local addresses, use a default scope ID
                // In practice, this should be determined from the interface
                0
            } else {
                0
            };
            SocketAddr::V6(multicast_endpoint_v6(scope_id))
        }
    }
}

/// Check if an IP address is IPv4
pub fn is_ipv4(addr: &IpAddr) -> bool {
    matches!(addr, IpAddr::V4(_))
}

/// Check if an IP address is IPv6
pub fn is_ipv6(addr: &IpAddr) -> bool {
    matches!(addr, IpAddr::V6(_))
}

/// Convert an IP address to a socket address with the Link port
pub fn to_socket_addr(addr: IpAddr) -> SocketAddr {
    SocketAddr::new(addr, crate::discovery::LINK_PORT)
}

/// Get the IP version string for logging
pub fn ip_version_string(addr: &IpAddr) -> &'static str {
    match addr {
        IpAddr::V4(_) => "IPv4",
        IpAddr::V6(_) => "IPv6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_multicast_endpoints() {
        // Test IPv4 multicast endpoint
        let v4_endpoint = multicast_endpoint_v4();
        assert_eq!(v4_endpoint.ip(), &crate::discovery::MULTICAST_ADDR);
        assert_eq!(v4_endpoint.port(), crate::discovery::LINK_PORT);

        // Test IPv6 multicast endpoint
        let v6_endpoint = multicast_endpoint_v6(1);
        assert_eq!(v6_endpoint.ip(), &MULTICAST_ADDR_V6);
        assert_eq!(v6_endpoint.port(), crate::discovery::LINK_PORT);
        assert_eq!(v6_endpoint.scope_id(), 1);
    }

    #[test]
    fn test_multicast_endpoint_for_addr() {
        let ipv4_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        let ipv6_addr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

        let v4_endpoint = multicast_endpoint_for_addr(&ipv4_addr);
        let v6_endpoint = multicast_endpoint_for_addr(&ipv6_addr);

        assert!(v4_endpoint.is_ipv4());
        assert!(v6_endpoint.is_ipv6());
    }

    #[test]
    fn test_ip_version_detection() {
        let ipv4_addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        let ipv6_addr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));

        assert!(is_ipv4(&ipv4_addr));
        assert!(!is_ipv6(&ipv4_addr));
        assert!(!is_ipv4(&ipv6_addr));
        assert!(is_ipv6(&ipv6_addr));
    }
}
