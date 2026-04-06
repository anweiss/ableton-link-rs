// Platform-specific network interface scanning optimizations
// Based on Ableton Link's interface detection implementations
// Now using 100% safe Rust implementations!

use network_interface::NetworkInterfaceConfig;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tracing::{debug, error};

/// Safe Rust implementation using the network-interface crate
/// This provides cross-platform network interface scanning without unsafe code
pub async fn scan_network_interfaces() -> Vec<IpAddr> {
    tokio::task::spawn_blocking(scan_interfaces_safe)
        .await
        .unwrap_or_else(|e| {
            error!("Failed to scan network interfaces: {}", e);
            Vec::new()
        })
}

// Safe implementation using the network-interface crate
fn scan_interfaces_safe() -> Vec<IpAddr> {
    use std::collections::HashMap;

    let mut addresses = Vec::new();
    let mut ipv4_interfaces: HashMap<String, IpAddr> = HashMap::new();

    // Get network interfaces using the safe network-interface crate
    match network_interface::NetworkInterface::show() {
        Ok(interfaces) => {
            // First pass: collect IPv4 addresses
            for interface in &interfaces {
                if !interface.addr.is_empty() {
                    for addr in &interface.addr {
                        let ip_addr = addr.ip();

                        if let IpAddr::V4(ipv4) = ip_addr {
                            if is_usable_ipv4(&ipv4) {
                                addresses.push(ip_addr);
                                ipv4_interfaces.insert(interface.name.clone(), ip_addr);
                                debug!("Found IPv4 interface: {} ({})", interface.name, ip_addr);
                            }
                        }
                    }
                }
            }

            // Second pass: collect IPv6 addresses only for interfaces that have IPv4
            for interface in &interfaces {
                if ipv4_interfaces.contains_key(&interface.name) && !interface.addr.is_empty() {
                    for addr in &interface.addr {
                        let ip_addr = addr.ip();

                        if let IpAddr::V6(ipv6) = ip_addr {
                            if is_usable_ipv6(&ipv6) {
                                addresses.push(ip_addr);
                                debug!("Found IPv6 interface: {} ({})", interface.name, ip_addr);
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            error!(
                "Failed to get network interfaces using network-interface crate: {}",
                e
            );

            // Fallback to if-addrs crate for better compatibility
            match if_addrs::get_if_addrs() {
                Ok(interfaces) => {
                    for interface in interfaces {
                        let addr = interface.ip();
                        if is_usable_interface(&addr) {
                            addresses.push(addr);
                            debug!("Found fallback interface: {} ({})", interface.name, addr);
                        }
                    }
                }
                Err(e2) => {
                    error!("Both network-interface and if-addrs failed: {} / {}", e, e2);
                }
            }
        }
    }

    // Sort and deduplicate
    addresses.sort();
    addresses.dedup();

    debug!(
        "Found {} total network interfaces using safe implementation",
        addresses.len()
    );
    addresses
}

/// Check if an IP address represents a usable interface for Link discovery
/// This mirrors the logic from the C++ implementation
fn is_usable_interface(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(ipv4) => is_usable_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_usable_ipv6(ipv6),
    }
}

/// Check if an IPv4 address is usable for Link discovery
fn is_usable_ipv4(addr: &Ipv4Addr) -> bool {
    // Exclude loopback, broadcast, and other special addresses
    !addr.is_loopback()
        && !addr.is_broadcast()
        && !addr.is_multicast()
        && !addr.is_unspecified()
        && !addr.is_link_local()
        && *addr != Ipv4Addr::new(0, 0, 0, 0)
}

/// Check if an IPv6 address is usable for Link discovery  
fn is_usable_ipv6(addr: &Ipv6Addr) -> bool {
    // For IPv6, we only want link-local addresses (as per C++ implementation)
    !addr.is_loopback() && addr.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_safe_interface_scanning() {
        let interfaces = scan_network_interfaces().await;
        // Should find at least one interface on most systems
        // Note: This might fail in containerized environments
        println!(
            "Found {} interfaces using safe implementation: {:?}",
            interfaces.len(),
            interfaces
        );

        // Verify all returned interfaces are usable
        for interface in &interfaces {
            assert!(
                is_usable_interface(interface),
                "Interface {:?} should be usable",
                interface
            );
        }
    }

    #[test]
    fn test_safe_usable_interface_detection() {
        // IPv4 tests
        assert!(!is_usable_ipv4(&Ipv4Addr::LOCALHOST)); // 127.0.0.1
        assert!(!is_usable_ipv4(&Ipv4Addr::UNSPECIFIED)); // 0.0.0.0
        assert!(!is_usable_ipv4(&Ipv4Addr::BROADCAST)); // 255.255.255.255
        assert!(is_usable_ipv4(&Ipv4Addr::new(192, 168, 1, 100))); // Private network

        // IPv6 tests
        assert!(!is_usable_ipv6(&Ipv6Addr::LOCALHOST)); // ::1
        assert!(!is_usable_ipv6(&Ipv6Addr::UNSPECIFIED)); // ::

        // Link-local IPv6 should be usable
        let link_local = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        assert!(is_usable_ipv6(&link_local));
    }

    #[test]
    fn test_safe_interface_filtering() {
        // Test various IP addresses to ensure proper filtering
        let test_addresses = [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), // Loopback - should be filtered
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),   // Unspecified - should be filtered
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)), // Private - should be kept
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),  // Private - should be kept
            IpAddr::V6(Ipv6Addr::LOCALHOST),         // IPv6 loopback - should be filtered
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)), // Link-local - should be kept
        ];

        let expected_usable = [
            false, // 127.0.0.1
            false, // 0.0.0.0
            true,  // 192.168.1.100
            true,  // 10.0.0.1
            false, // IPv6 ::1
            true,  // IPv6 link-local
        ];

        for (addr, expected) in test_addresses.iter().zip(expected_usable.iter()) {
            assert_eq!(
                is_usable_interface(addr),
                *expected,
                "Interface {:?} usability should be {}",
                addr,
                expected
            );
        }
    }

    #[test]
    fn test_network_interface_crate_availability() {
        // Test that the network-interface crate is working
        match network_interface::NetworkInterface::show() {
            Ok(interfaces) => {
                println!(
                    "network-interface crate found {} interfaces",
                    interfaces.len()
                );
                // Just verify we can access the interface data safely
                for interface in interfaces.iter().take(3) {
                    // Limit to first 3 for testing
                    println!(
                        "Interface: {} with {} addresses",
                        interface.name,
                        interface.addr.len()
                    );
                }
            }
            Err(e) => {
                println!(
                    "network-interface crate error (may be expected in some environments): {}",
                    e
                );
                // This is okay - the code will fallback to if-addrs
            }
        }
    }

    #[test]
    fn test_fallback_interface_scanning() {
        // Test the fallback if-addrs implementation
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                println!("if-addrs fallback found {} interfaces", interfaces.len());
                for interface in interfaces.iter().take(3) {
                    // Limit for testing
                    println!(
                        "Fallback interface: {} ({})",
                        interface.name,
                        interface.ip()
                    );
                    // Verify we can safely access the data
                    assert!(!interface.name.is_empty());
                }
            }
            Err(e) => {
                println!(
                    "if-addrs error (may be expected in some environments): {}",
                    e
                );
            }
        }
    }
}
