use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use tokio::{sync::Notify, time::interval};
use tracing::{debug, error, info};

/// Network interface scanner that monitors available network interfaces
/// and notifies when interfaces change
pub struct InterfaceScanner {
    scan_period: Duration,
    cancel_notify: Arc<Notify>,
    callback: Arc<dyn Fn(Vec<IpAddr>) + Send + Sync>,
}

impl InterfaceScanner {
    /// Create a new interface scanner with the specified scan period and callback
    pub fn new<F>(scan_period: Duration, callback: F) -> Self
    where
        F: Fn(Vec<IpAddr>) + Send + Sync + 'static,
    {
        Self {
            scan_period,
            cancel_notify: Arc::new(Notify::new()),
            callback: Arc::new(callback),
        }
    }

    /// Enable or disable the interface scanner
    pub async fn enable(&self, enable: bool) {
        if enable {
            self.start_scanning().await;
        } else {
            self.cancel_notify.notify_one();
        }
    }

    /// Perform a single scan of network interfaces
    pub async fn scan_once(&self) {
        let interfaces = scan_network_interfaces().await;
        (self.callback)(interfaces);
    }

    /// Start continuous scanning
    async fn start_scanning(&self) {
        let mut interval = interval(self.scan_period);
        let cancel_notify = self.cancel_notify.clone();
        let callback = self.callback.clone();

        tokio::spawn(async move {
            let mut last_interfaces: Option<HashSet<IpAddr>> = None;

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        debug!("Scanning network interfaces");
                        let interfaces = scan_network_interfaces().await;
                        let interface_set: HashSet<IpAddr> = interfaces.iter().copied().collect();

                        // Only call callback if interfaces have changed
                        if last_interfaces.as_ref() != Some(&interface_set) {
                            info!("Network interfaces changed: {:?}", interfaces);
                            callback(interfaces);
                            last_interfaces = Some(interface_set);
                        }
                    }
                    _ = cancel_notify.notified() => {
                        debug!("Interface scanner cancelled");
                        break;
                    }
                }
            }
        });
    }
}

/// Scan for available network interfaces that can be used for Link discovery
pub async fn scan_network_interfaces() -> Vec<IpAddr> {
    let mut interfaces = Vec::new();

    match get_network_interfaces().await {
        Ok(addrs) => {
            for addr in addrs {
                if is_usable_interface(&addr) {
                    interfaces.push(addr);
                }
            }
        }
        Err(e) => {
            error!("Failed to scan network interfaces: {}", e);
        }
    }

    // Sort and deduplicate
    interfaces.sort();
    interfaces.dedup();

    debug!("Found {} usable network interfaces", interfaces.len());
    interfaces
}

/// Get all network interfaces from the system
async fn get_network_interfaces() -> Result<Vec<IpAddr>, Box<dyn std::error::Error + Send + Sync>> {
    // Use tokio's blocking task to avoid blocking the async runtime
    tokio::task::spawn_blocking(|| {
        let mut addresses = Vec::new();

        // Use the `if-addrs` crate for cross-platform interface detection
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                for interface in interfaces {
                    let addr = interface.ip();
                    addresses.push(addr);
                    debug!("Found interface: {} ({})", interface.name, addr);
                }
            }
            Err(e) => {
                return Err(format!("Failed to get network interfaces: {}", e).into());
            }
        }

        Ok(addresses)
    })
    .await?
}

/// Check if an IP address represents a usable interface for Link discovery
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
    // Exclude loopback, multicast, and other special addresses
    // Allow link-local addresses for IPv6 as they're common in local networks
    !addr.is_loopback()
        && !addr.is_multicast()
        && !addr.is_unspecified()
        && *addr != Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1) // ::1
}

/// Get the scope ID for an IPv6 address (needed for link-local addresses)
pub fn get_ipv6_scope_id(addr: &Ipv6Addr) -> Option<u32> {
    if addr.is_unicast_link_local() {
        // For link-local addresses, we need to determine the scope ID
        // This is a simplified implementation - in practice, you'd want to
        // get the actual interface index from the system
        Some(0) // Default scope
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_interface_scanning() {
        let _ = tracing_subscriber::fmt::try_init();

        let interfaces = Arc::new(Mutex::new(Vec::new()));
        let interfaces_clone = interfaces.clone();

        let scanner = InterfaceScanner::new(Duration::from_millis(100), move |addrs| {
            *interfaces_clone.lock().unwrap() = addrs;
        });

        // Test single scan
        scanner.scan_once().await;

        let scanned_interfaces = interfaces.lock().unwrap().clone();
        assert!(
            !scanned_interfaces.is_empty(),
            "Should find at least one interface"
        );

        // Verify we found some usable interfaces
        for addr in &scanned_interfaces {
            assert!(
                is_usable_interface(addr),
                "All returned interfaces should be usable"
            );
        }

        info!(
            "Found {} interfaces: {:?}",
            scanned_interfaces.len(),
            scanned_interfaces
        );
    }

    #[test]
    fn test_usable_interface_detection() {
        // IPv4 tests
        assert!(!is_usable_ipv4(&Ipv4Addr::LOCALHOST)); // 127.0.0.1
        assert!(!is_usable_ipv4(&Ipv4Addr::UNSPECIFIED)); // 0.0.0.0
        assert!(!is_usable_ipv4(&Ipv4Addr::BROADCAST)); // 255.255.255.255
        assert!(is_usable_ipv4(&Ipv4Addr::new(192, 168, 1, 100))); // Private network

        // IPv6 tests
        assert!(!is_usable_ipv6(&Ipv6Addr::LOCALHOST)); // ::1
        assert!(!is_usable_ipv6(&Ipv6Addr::UNSPECIFIED)); // ::
        assert!(is_usable_ipv6(&Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))); // Link-local
        assert!(is_usable_ipv6(&Ipv6Addr::new(
            0x2001, 0xdb8, 0, 0, 0, 0, 0, 1
        ))); // Global unicast
    }
}
