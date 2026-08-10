//! The set of peers currently requesting a sink's channel.
//!
//! Ported from `ableton/link_audio/Receivers.hpp`. Requests carry a ttl and
//! are refreshed periodically by the requesting peer; entries that are not
//! refreshed in time are pruned.

use std::{
    net::SocketAddrV4,
    time::{Duration, Instant},
};

use super::payload::{ChannelRequest, ChannelStopRequest, Id};

/// Padding added before an expired receiver is dropped, matching upstream's
/// one second timer padding.
const PRUNE_PADDING: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Receiver {
    peer_id: Id,
    endpoint: Option<SocketAddrV4>,
    expires_at: Instant,
}

#[derive(Debug, Default)]
pub struct Receivers {
    // Invariant: sorted by expiry.
    receivers: Vec<Receiver>,
}

impl Receivers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers or refreshes a channel request.
    pub fn receive_channel_request(
        &mut self,
        request: &ChannelRequest,
        ttl: u8,
        endpoint: Option<SocketAddrV4>,
        now: Instant,
    ) {
        self.receivers.retain(|r| r.peer_id != request.peer_id);

        let receiver = Receiver {
            peer_id: request.peer_id,
            endpoint,
            expires_at: now + Duration::from_secs(u64::from(ttl)),
        };
        let index = self
            .receivers
            .partition_point(|r| r.expires_at <= receiver.expires_at);
        self.receivers.insert(index, receiver);
    }

    /// Removes a peer that asked to stop receiving the channel.
    pub fn receive_channel_stop_request(&mut self, request: &ChannelStopRequest) {
        self.receivers.retain(|r| r.peer_id != request.peer_id);
    }

    /// Drops receivers whose requests have not been refreshed.
    pub fn prune_expired(&mut self, now: Instant) {
        self.receivers
            .retain(|r| r.expires_at + PRUNE_PADDING > now);
    }

    /// Removes receivers that are no longer session peers.
    pub fn retain_peers(&mut self, peers: &[Id]) {
        self.receivers.retain(|r| peers.contains(&r.peer_id));
    }

    /// Updates the endpoint used to reach a peer, e.g. after its announcement
    /// arrived on a better gateway.
    pub fn set_endpoint(&mut self, peer_id: Id, endpoint: Option<SocketAddrV4>) {
        for receiver in self.receivers.iter_mut().filter(|r| r.peer_id == peer_id) {
            receiver.endpoint = endpoint;
        }
    }

    /// The endpoints audio should be sent to.
    pub fn endpoints(&self) -> impl Iterator<Item = SocketAddrV4> + '_ {
        self.receivers.iter().filter_map(|r| r.endpoint)
    }

    pub fn is_empty(&self) -> bool {
        self.receivers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.receivers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn endpoint(last: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, last), 20808)
    }

    fn request(peer: u8) -> ChannelRequest {
        ChannelRequest {
            peer_id: Id::from_array([peer; 8]),
            channel_id: Id::from_array([0xaa; 8]),
        }
    }

    #[test]
    fn requests_register_endpoints() {
        let mut receivers = Receivers::new();
        let now = Instant::now();
        receivers.receive_channel_request(&request(1), 5, Some(endpoint(1)), now);
        receivers.receive_channel_request(&request(2), 5, Some(endpoint(2)), now);

        assert_eq!(receivers.len(), 2);
        let endpoints: Vec<_> = receivers.endpoints().collect();
        assert!(endpoints.contains(&endpoint(1)));
        assert!(endpoints.contains(&endpoint(2)));
    }

    #[test]
    fn repeated_requests_refresh_rather_than_duplicate() {
        let mut receivers = Receivers::new();
        let now = Instant::now();
        receivers.receive_channel_request(&request(1), 5, Some(endpoint(1)), now);
        receivers.receive_channel_request(
            &request(1),
            5,
            Some(endpoint(1)),
            now + Duration::from_secs(2),
        );
        assert_eq!(receivers.len(), 1);

        receivers.prune_expired(now + Duration::from_secs(7));
        assert_eq!(receivers.len(), 1);
    }

    #[test]
    fn stop_requests_remove_the_peer() {
        let mut receivers = Receivers::new();
        receivers.receive_channel_request(&request(1), 5, Some(endpoint(1)), Instant::now());
        receivers.receive_channel_stop_request(&ChannelStopRequest {
            peer_id: Id::from_array([1; 8]),
            channel_id: Id::from_array([0xaa; 8]),
        });
        assert!(receivers.is_empty());
    }

    #[test]
    fn expired_requests_are_pruned() {
        let mut receivers = Receivers::new();
        let now = Instant::now();
        receivers.receive_channel_request(&request(1), 5, Some(endpoint(1)), now);
        receivers.prune_expired(now + Duration::from_secs(5));
        assert_eq!(receivers.len(), 1);
        receivers.prune_expired(now + Duration::from_secs(7));
        assert!(receivers.is_empty());
    }

    #[test]
    fn receivers_are_pruned_when_peers_leave() {
        let mut receivers = Receivers::new();
        let now = Instant::now();
        receivers.receive_channel_request(&request(1), 5, Some(endpoint(1)), now);
        receivers.receive_channel_request(&request(2), 5, Some(endpoint(2)), now);

        receivers.retain_peers(&[Id::from_array([2; 8])]);
        assert_eq!(receivers.len(), 1);
        assert_eq!(receivers.endpoints().next(), Some(endpoint(2)));
    }

    #[test]
    fn endpoints_can_be_updated() {
        let mut receivers = Receivers::new();
        receivers.receive_channel_request(&request(1), 5, None, Instant::now());
        assert_eq!(receivers.endpoints().count(), 0);
        receivers.set_endpoint(Id::from_array([1; 8]), Some(endpoint(1)));
        assert_eq!(receivers.endpoints().next(), Some(endpoint(1)));
    }
}
