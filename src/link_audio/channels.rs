//! The registry of audio channels discovered in the Link session.
//!
//! Ported from `ableton/link_audio/Channels.hpp`. Channels are learned from
//! peer announcements, tracked per gateway, expired by ttl, and removed when
//! the announcing peer leaves the session or says goodbye.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    time::{Duration, Instant},
};

use crate::link::sessions::SessionId;

use super::payload::Id;

/// Padding added before an entry expires, matching upstream's one second
/// timer padding.
const PRUNE_PADDING: Duration = Duration::from_secs(1);

/// An audio channel available in the Link session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub peer_id: Id,
    pub peer_name: String,
    pub session_id: SessionId,
}

#[derive(Debug, Clone)]
struct ChannelInfo {
    channel: Channel,
    gateway_addr: Ipv4Addr,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct PeerSendHandler {
    endpoint: SocketAddrV4,
    network_quality: f64,
}

/// Everything known about the audio channels of the current session.
#[derive(Debug, Default)]
pub struct Channels {
    channels: Vec<ChannelInfo>,
    peer_send_handlers: HashMap<Id, PeerSendHandler>,
}

/// A single channel as announced by a peer.
#[derive(Debug, Clone)]
pub struct AnnouncedChannel {
    pub id: Id,
    pub name: String,
}

impl Channels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the channels announced by a peer on a gateway. Returns `true`
    /// if the visible set of channels changed.
    #[allow(clippy::too_many_arguments)]
    pub fn saw_announcement(
        &mut self,
        peer_id: Id,
        peer_name: &str,
        session_id: SessionId,
        announced: &[AnnouncedChannel],
        gateway_addr: Ipv4Addr,
        from: SocketAddrV4,
        network_quality: f64,
        ttl: u8,
        now: Instant,
    ) -> bool {
        let expires_at = now + Duration::from_secs(u64::from(ttl));
        let mut changed = false;

        for announced in announced {
            let channel = Channel {
                id: announced.id,
                name: announced.name.clone(),
                peer_id,
                peer_name: peer_name.to_string(),
                session_id,
            };

            match self
                .channels
                .iter_mut()
                .find(|info| info.channel.id == channel.id && info.gateway_addr == gateway_addr)
            {
                Some(info) => {
                    if info.channel != channel {
                        changed = true;
                        info.channel = channel;
                    }
                    info.expires_at = expires_at;
                }
                None => {
                    changed = true;
                    self.channels.push(ChannelInfo {
                        channel,
                        gateway_addr,
                        expires_at,
                    });
                }
            }
        }

        // Prefer the gateway with the best measured link quality.
        match self.peer_send_handlers.get(&peer_id) {
            Some(existing) if existing.network_quality >= network_quality => {}
            _ => {
                self.peer_send_handlers.insert(
                    peer_id,
                    PeerSendHandler {
                        endpoint: from,
                        network_quality,
                    },
                );
            }
        }

        changed
    }

    /// Removes channels a peer said goodbye to on a gateway.
    pub fn channels_left(&mut self, gateway_addr: Ipv4Addr, byes: &[Id]) -> bool {
        let before = self.channels.len();
        self.channels
            .retain(|info| !(info.gateway_addr == gateway_addr && byes.contains(&info.channel.id)));
        let changed = self.channels.len() != before;
        if changed {
            self.prune_send_handlers();
        }
        changed
    }

    /// Removes every channel learned on a gateway that has gone away.
    pub fn gateway_closed(&mut self, gateway_addr: Ipv4Addr) -> bool {
        let before = self.channels.len();
        self.channels
            .retain(|info| info.gateway_addr != gateway_addr);
        let changed = self.channels.len() != before;
        if changed {
            self.prune_send_handlers();
        }
        changed
    }

    /// Removes channels belonging to peers that are no longer in the session.
    pub fn prune_peer_channels(&mut self, connected_peers: &[Id]) -> bool {
        let before = self.channels.len();
        self.channels
            .retain(|info| connected_peers.contains(&info.channel.peer_id));
        let changed = self.channels.len() != before;
        self.peer_send_handlers
            .retain(|peer_id, _| connected_peers.contains(peer_id));
        changed
    }

    /// Removes channels whose announcements have not been refreshed.
    pub fn prune_expired(&mut self, now: Instant) -> bool {
        let before = self.channels.len();
        self.channels
            .retain(|info| info.expires_at + PRUNE_PADDING > now);
        let changed = self.channels.len() != before;
        if changed {
            self.prune_send_handlers();
        }
        changed
    }

    fn prune_send_handlers(&mut self) {
        let peers: Vec<Id> = self.channels.iter().map(|c| c.channel.peer_id).collect();
        self.peer_send_handlers
            .retain(|peer_id, _| peers.contains(peer_id));
    }

    /// The channels of a session, de-duplicated across gateways.
    pub fn session_channels(&self, session_id: SessionId) -> Vec<Channel> {
        let mut result: Vec<Channel> = Vec::new();
        for info in &self.channels {
            if info.channel.session_id == session_id
                && !result.iter().any(|c| c.id == info.channel.id)
            {
                result.push(info.channel.clone());
            }
        }
        result
    }

    /// All known channels, de-duplicated across gateways.
    pub fn all_channels(&self) -> Vec<Channel> {
        let mut result: Vec<Channel> = Vec::new();
        for info in &self.channels {
            if !result.iter().any(|c| c.id == info.channel.id) {
                result.push(info.channel.clone());
            }
        }
        result
    }

    /// The endpoint to reach a peer on.
    pub fn peer_endpoint(&self, peer_id: Id) -> Option<SocketAddrV4> {
        self.peer_send_handlers.get(&peer_id).map(|h| h.endpoint)
    }

    /// The endpoint to reach the peer publishing a channel.
    pub fn channel_endpoint(&self, channel_id: Id) -> Option<SocketAddrV4> {
        let peer_id = self
            .channels
            .iter()
            .find(|info| info.channel.id == channel_id)
            .map(|info| info.channel.peer_id)?;
        self.peer_endpoint(peer_id)
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::node::NodeId;

    fn id(n: u8) -> Id {
        NodeId::from_array([n; 8])
    }

    fn session(n: u8) -> SessionId {
        SessionId(NodeId::from_array([n; 8]))
    }

    fn endpoint(last: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, last), 20808)
    }

    fn gateway() -> Ipv4Addr {
        Ipv4Addr::new(192, 168, 1, 10)
    }

    fn announce(
        channels: &mut Channels,
        peer: u8,
        peer_name: &str,
        names: &[(u8, &str)],
        quality: f64,
        now: Instant,
    ) -> bool {
        let announced: Vec<AnnouncedChannel> = names
            .iter()
            .map(|(n, name)| AnnouncedChannel {
                id: id(*n),
                name: name.to_string(),
            })
            .collect();
        channels.saw_announcement(
            id(peer),
            peer_name,
            session(0),
            &announced,
            gateway(),
            endpoint(peer),
            quality,
            5,
            now,
        )
    }

    #[test]
    fn announcements_add_channels() {
        let mut channels = Channels::new();
        let now = Instant::now();
        assert!(announce(
            &mut channels,
            1,
            "Live",
            &[(10, "drums"), (11, "bass")],
            1.0,
            now
        ));
        assert!(!announce(
            &mut channels,
            1,
            "Live",
            &[(10, "drums"), (11, "bass")],
            1.0,
            now
        ));

        let session_channels = channels.session_channels(session(0));
        assert_eq!(session_channels.len(), 2);
        assert_eq!(session_channels[0].name, "drums");
        assert_eq!(session_channels[0].peer_name, "Live");
        assert_eq!(channels.peer_endpoint(id(1)), Some(endpoint(1)));
        assert_eq!(channels.channel_endpoint(id(11)), Some(endpoint(1)));
    }

    #[test]
    fn renaming_a_channel_is_a_change() {
        let mut channels = Channels::new();
        let now = Instant::now();
        announce(&mut channels, 1, "Live", &[(10, "drums")], 1.0, now);
        assert!(announce(
            &mut channels,
            1,
            "Live",
            &[(10, "beats")],
            1.0,
            now
        ));
        assert_eq!(channels.session_channels(session(0))[0].name, "beats");
    }

    #[test]
    fn better_gateways_win() {
        let mut channels = Channels::new();
        let now = Instant::now();
        announce(&mut channels, 1, "Live", &[(10, "drums")], 1.0, now);
        assert_eq!(channels.peer_endpoint(id(1)), Some(endpoint(1)));

        channels.saw_announcement(
            id(1),
            "Live",
            session(0),
            &[AnnouncedChannel {
                id: id(10),
                name: "drums".into(),
            }],
            gateway(),
            endpoint(99),
            5.0,
            5,
            now,
        );
        assert_eq!(channels.peer_endpoint(id(1)), Some(endpoint(99)));

        // A worse gateway does not take over.
        channels.saw_announcement(
            id(1),
            "Live",
            session(0),
            &[AnnouncedChannel {
                id: id(10),
                name: "drums".into(),
            }],
            gateway(),
            endpoint(1),
            0.5,
            5,
            now,
        );
        assert_eq!(channels.peer_endpoint(id(1)), Some(endpoint(99)));
    }

    #[test]
    fn byes_remove_channels() {
        let mut channels = Channels::new();
        announce(
            &mut channels,
            1,
            "Live",
            &[(10, "drums"), (11, "bass")],
            1.0,
            Instant::now(),
        );
        assert!(channels.channels_left(gateway(), &[id(10)]));
        assert_eq!(channels.session_channels(session(0)).len(), 1);
        assert!(!channels.channels_left(gateway(), &[id(10)]));
    }

    #[test]
    fn expired_channels_are_pruned() {
        let mut channels = Channels::new();
        let now = Instant::now();
        announce(&mut channels, 1, "Live", &[(10, "drums")], 1.0, now);

        assert!(!channels.prune_expired(now + Duration::from_secs(5)));
        assert!(channels.prune_expired(now + Duration::from_secs(7)));
        assert!(channels.is_empty());
        assert_eq!(channels.peer_endpoint(id(1)), None);
    }

    #[test]
    fn channels_of_departed_peers_are_removed() {
        let mut channels = Channels::new();
        let now = Instant::now();
        announce(&mut channels, 1, "Live", &[(10, "drums")], 1.0, now);
        announce(&mut channels, 2, "Bitwig", &[(20, "synth")], 1.0, now);

        assert!(channels.prune_peer_channels(&[id(2)]));
        assert_eq!(channels.all_channels().len(), 1);
        assert_eq!(channels.peer_endpoint(id(1)), None);
        assert_eq!(channels.peer_endpoint(id(2)), Some(endpoint(2)));
    }

    #[test]
    fn closing_a_gateway_removes_its_channels() {
        let mut channels = Channels::new();
        announce(
            &mut channels,
            1,
            "Live",
            &[(10, "drums")],
            1.0,
            Instant::now(),
        );
        assert!(channels.gateway_closed(gateway()));
        assert!(channels.is_empty());
        assert!(!channels.gateway_closed(gateway()));
    }

    #[test]
    fn channels_of_other_sessions_are_not_listed() {
        let mut channels = Channels::new();
        announce(
            &mut channels,
            1,
            "Live",
            &[(10, "drums")],
            1.0,
            Instant::now(),
        );
        assert!(channels.session_channels(session(9)).is_empty());
        assert_eq!(channels.all_channels().len(), 1);
    }
}
