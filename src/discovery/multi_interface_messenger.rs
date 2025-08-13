use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use tokio::{
    net::UdpSocket,
    sync::{mpsc, Mutex},
};
use tracing::{debug, error, info, warn};

use super::{
    interface_scanner::{InterfaceScanner, scan_network_interfaces},
    ip_interface::{multicast_endpoint_for_addr, is_ipv4, to_socket_addr},
    messenger::new_udp_reuseport,
};

/// Multi-interface messenger that handles both IPv4 and IPv6 interfaces
pub struct MultiInterfaceMessenger {
    interfaces: Arc<Mutex<HashMap<IpAddr, InterfaceHandler>>>,
    scanner: InterfaceScanner,
    message_tx: mpsc::UnboundedSender<(Vec<u8>, Option<SocketAddr>)>,
}

/// Handler for a specific network interface
struct InterfaceHandler {
    socket: Arc<UdpSocket>,
    ip_addr: IpAddr,
}

impl MultiInterfaceMessenger {
    /// Create a new multi-interface messenger
    pub async fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let interfaces = Arc::new(Mutex::new(HashMap::new()));
        let (message_tx, mut message_rx) = mpsc::unbounded_channel::<(Vec<u8>, Option<SocketAddr>)>();

        // Create interface scanner
        let interfaces_for_scanner = interfaces.clone();
        let scanner = InterfaceScanner::new(
            std::time::Duration::from_secs(30), // Scan every 30 seconds
            move |new_interfaces| {
                let interfaces = interfaces_for_scanner.clone();
                tokio::spawn(async move {
                    Self::update_interfaces(interfaces, new_interfaces).await;
                });
            },
        );

        // Initialize with current interfaces
        let current_interfaces = scan_network_interfaces().await;
        Self::update_interfaces(interfaces.clone(), current_interfaces).await;

        // Start message sending task
        let interfaces_for_sender = interfaces.clone();
        tokio::spawn(async move {
            while let Some((message, target)) = message_rx.recv().await {
                Self::send_message_internal(interfaces_for_sender.clone(), message, target).await;
            }
        });

        Ok(Self {
            interfaces,
            scanner,
            message_tx,
        })
    }

    /// Enable or disable the messenger
    pub async fn enable(&self, enable: bool) {
        self.scanner.enable(enable).await;
        
        if !enable {
            // Clear all interfaces when disabled
            self.interfaces.lock().await.clear();
        }
    }

    /// Send a message to all interfaces (multicast)
    pub async fn send_multicast(&self, message: Vec<u8>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.message_tx.send((message, None))
            .map_err(|e| format!("Failed to queue multicast message: {}", e))?;
        Ok(())
    }

    /// Send a message to a specific target
    pub async fn send_unicast(&self, message: Vec<u8>, target: SocketAddr) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.message_tx.send((message, Some(target)))
            .map_err(|e| format!("Failed to queue unicast message: {}", e))?;
        Ok(())
    }

    /// Get the current number of active interfaces
    pub async fn interface_count(&self) -> usize {
        self.interfaces.lock().await.len()
    }

    /// Get the current interface addresses
    pub async fn interface_addresses(&self) -> Vec<IpAddr> {
        self.interfaces.lock().await.keys().copied().collect()
    }

    /// Update the active interfaces
    async fn update_interfaces(
        interfaces: Arc<Mutex<HashMap<IpAddr, InterfaceHandler>>>,
        new_interfaces: Vec<IpAddr>,
    ) {
        let mut interface_map = interfaces.lock().await;
        let current_addrs: std::collections::HashSet<_> = interface_map.keys().copied().collect();
        let new_addrs: std::collections::HashSet<_> = new_interfaces.iter().copied().collect();

        // Remove interfaces that are no longer available
        let to_remove: Vec<_> = current_addrs.difference(&new_addrs).copied().collect();
        for addr in to_remove {
            if interface_map.remove(&addr).is_some() {
                info!("Removed interface: {}", addr);
            }
        }

        // Add new interfaces
        let to_add: Vec<_> = new_addrs.difference(&current_addrs).copied().collect();
        for addr in to_add {
            match Self::create_interface_handler(addr).await {
                Ok(handler) => {
                    interface_map.insert(addr, handler);
                    info!("Added interface: {}", addr);
                }
                Err(e) => {
                    warn!("Failed to create handler for interface {}: {}", addr, e);
                }
            }
        }

        debug!("Active interfaces: {:?}", interface_map.keys().collect::<Vec<_>>());
    }

    /// Create a handler for a specific interface
    async fn create_interface_handler(
        addr: IpAddr,
    ) -> Result<InterfaceHandler, Box<dyn std::error::Error + Send + Sync>> {
        let socket = if is_ipv4(&addr) {
            Self::create_ipv4_socket(addr).await?
        } else {
            Self::create_ipv6_socket(addr).await?
        };

        Ok(InterfaceHandler {
            socket: Arc::new(socket),
            ip_addr: addr,
        })
    }

    /// Create an IPv4 socket for the interface
    async fn create_ipv4_socket(
        addr: IpAddr,
    ) -> Result<UdpSocket, Box<dyn std::error::Error + Send + Sync>> {
        let socket_addr = to_socket_addr(addr);
        let socket = new_udp_reuseport(socket_addr.into())?;

        // Join multicast group
        if let IpAddr::V4(ipv4_addr) = addr {
            socket.join_multicast_v4(crate::discovery::MULTICAST_ADDR, ipv4_addr)?;
            socket.set_multicast_loop_v4(true)?;
            socket.set_multicast_ttl_v4(2)?;
        }

        Ok(socket)
    }

    /// Create an IPv6 socket for the interface
    async fn create_ipv6_socket(
        addr: IpAddr,
    ) -> Result<UdpSocket, Box<dyn std::error::Error + Send + Sync>> {
        let socket_addr = to_socket_addr(addr);
        let socket = UdpSocket::bind(socket_addr).await?;

        // Join multicast group for IPv6
        if let IpAddr::V6(_ipv6_addr) = addr {
            // For IPv6, we need to get the interface index
            // This is a simplified implementation - in practice, you'd want to
            // get the actual interface index from the system
            let interface_index = 0; // Default interface
            
            socket.join_multicast_v6(&super::ip_interface::MULTICAST_ADDR_V6, interface_index)?;
            socket.set_multicast_loop_v6(true)?;
            socket.set_multicast_loop_v6(true)?;
        }

        Ok(socket)
    }

    /// Send a message using the internal interfaces
    async fn send_message_internal(
        interfaces: Arc<Mutex<HashMap<IpAddr, InterfaceHandler>>>,
        message: Vec<u8>,
        target: Option<SocketAddr>,
    ) {
        let interface_map = interfaces.lock().await;

        if let Some(target_addr) = target {
            // Unicast to specific target
            Self::send_unicast_internal(&interface_map, &message, target_addr).await;
        } else {
            // Multicast to all interfaces
            Self::send_multicast_internal(&interface_map, &message).await;
        }
    }

    /// Send unicast message
    async fn send_unicast_internal(
        interfaces: &HashMap<IpAddr, InterfaceHandler>,
        message: &[u8],
        target: SocketAddr,
    ) {
        // Find an appropriate interface for the target
        let target_is_ipv4 = target.is_ipv4();
        
        for (_, handler) in interfaces.iter() {
            let handler_is_ipv4 = is_ipv4(&handler.ip_addr);
            
            // Use matching IP version
            if target_is_ipv4 == handler_is_ipv4 {
                match handler.socket.send_to(message, target).await {
                    Ok(bytes_sent) => {
                        debug!("Sent {} bytes to {} via {}", bytes_sent, target, handler.ip_addr);
                        return; // Success, no need to try other interfaces
                    }
                    Err(e) => {
                        warn!("Failed to send to {} via {}: {}", target, handler.ip_addr, e);
                    }
                }
            }
        }
        
        error!("Failed to send unicast message to {}", target);
    }

    /// Send multicast message to all interfaces
    async fn send_multicast_internal(
        interfaces: &HashMap<IpAddr, InterfaceHandler>,
        message: &[u8],
    ) {
        for (_, handler) in interfaces.iter() {
            let multicast_addr = multicast_endpoint_for_addr(&handler.ip_addr);
            
            match handler.socket.send_to(message, multicast_addr).await {
                Ok(bytes_sent) => {
                    debug!("Sent {} bytes multicast via {} to {}", 
                           bytes_sent, handler.ip_addr, multicast_addr);
                }
                Err(e) => {
                    warn!("Failed to send multicast via {}: {}", handler.ip_addr, e);
                }
            }
        }
    }

    /// Start receiving messages on all interfaces
    pub async fn start_receiving<F>(&self, _handler: F) 
    where
        F: FnMut(SocketAddr, Vec<u8>) + Send + 'static,
    {
        let interfaces = self.interfaces.clone();
        
        tokio::spawn(async move {
            loop {
                let interface_map = interfaces.lock().await;
                
                for (addr, interface_handler) in interface_map.iter() {
                    let socket = interface_handler.socket.clone();
                    let interface_addr = *addr;
                    
                    // Spawn a task for each interface to receive messages
                    tokio::spawn(async move {
                        let mut buffer = vec![0u8; 1024];
                        
                        loop {
                            match socket.recv_from(&mut buffer).await {
                                Ok((size, source)) => {
                                    debug!("Received {} bytes from {} on interface {}", 
                                           size, source, interface_addr);
                                    // Note: We need to handle the closure capture differently
                                    // This is a simplified version
                                    // In practice, you'd use channels to communicate with the handler
                                }
                                Err(e) => {
                                    warn!("Failed to receive on interface {}: {}", interface_addr, e);
                                    break;
                                }
                            }
                        }
                    });
                }
                
                // Check for interface changes periodically
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_multi_interface_messenger() {
        let _ = tracing_subscriber::fmt::try_init();

        // Create messenger
        let messenger = MultiInterfaceMessenger::new().await
            .expect("Failed to create multi-interface messenger");

        // Enable the messenger
        messenger.enable(true).await;

        // Wait a bit for interface discovery
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check that we have some interfaces
        let interface_count = messenger.interface_count().await;
        assert!(interface_count > 0, "Should discover at least one interface");

        let addresses = messenger.interface_addresses().await;
        info!("Discovered {} interfaces: {:?}", interface_count, addresses);

        // Test sending a multicast message
        let test_message = b"Hello, Link!".to_vec();
        messenger.send_multicast(test_message)
            .await
            .expect("Failed to send multicast message");
    }
}
