//! The public LinkAudio API.
//!
//! Ported from upstream `ableton/LinkAudio.hpp`. [`LinkAudio`] provides the
//! full [`BasicLink`] functionality plus audio sharing: channels are published
//! with a [`LinkAudioSink`] and consumed with a [`LinkAudioSource`]. Audio
//! buffers are interleaved 16-bit signed samples, and the Link beat grid plus
//! quantum are used to align audio coming from different peers.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration as StdDuration,
};

use local_ip_address::list_afinet_netifas;

use crate::link::{beats::Beats, BasicLink, SessionState};

use super::{
    beat_time_mapping::beat_at_global_beat,
    buffer::BufferInfo,
    channels::Channel,
    engine::{AudioEngine, ChannelsChangedCallback},
    payload::Id,
    sink::{Sink, SinkBufferHandle},
    source::Source,
};

/// How often the LinkAudio engine is refreshed with the peers and endpoints
/// discovered by Link Classic.
const PEER_SYNC_PERIOD: StdDuration = StdDuration::from_millis(250);

/// Link with audio sharing.
///
/// `LinkAudio` derefs to [`BasicLink`], so the entire Link API is available on
/// it. Use [`LinkAudio::enable_link_audio`] to start sharing audio.
pub struct LinkAudio {
    link: BasicLink,
    engine: Arc<AudioEngine>,
    sync_task: Option<tokio::task::JoinHandle<()>>,
}

impl LinkAudio {
    /// Constructs a LinkAudio instance with an initial tempo and the local peer
    /// name used for identification in the session. Names longer than 256
    /// bytes are truncated.
    pub async fn new(bpm: f64, name: impl Into<String>) -> std::io::Result<Self> {
        let link = BasicLink::new(bpm).await?;
        let addr = local_ipv4()?;
        let engine = Arc::new(
            AudioEngine::new(
                addr,
                link.controller().node_id(),
                link.controller().session_id(),
                name,
            )
            .await?,
        );

        Ok(LinkAudio {
            link,
            engine,
            sync_task: None,
        })
    }

    /// Is audio sharing currently enabled?
    pub fn is_link_audio_enabled(&self) -> bool {
        self.sync_task.is_some()
    }

    /// Enables or disables audio sharing. While enabled, this peer's audio
    /// endpoint is announced to the session in the Link `aep4` payload entry
    /// and peers' endpoints are tracked.
    pub fn enable_link_audio(&mut self, enable: bool) {
        if enable == self.is_link_audio_enabled() {
            return;
        }

        if enable {
            self.link
                .controller()
                .set_audio_endpoint(Some(self.engine.endpoint()));
            self.sync_task = Some(self.spawn_peer_sync());
        } else {
            self.link.controller().set_audio_endpoint(None);
            if let Some(task) = self.sync_task.take() {
                task.abort();
            }
            self.engine.update_session_peers(&[]);
        }
    }

    /// The local peer name used for identification in the session.
    pub fn peer_name(&self) -> String {
        self.engine.peer_name()
    }

    /// Changes the local peer name. Names longer than 256 bytes are truncated.
    pub fn set_peer_name(&self, name: impl Into<String>) {
        self.engine.set_peer_name(name);
    }

    /// Registers a callback invoked when channels are discovered, disappear,
    /// or are renamed. The callback is invoked on a Link-managed thread.
    pub fn set_channels_changed_callback<F>(&self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        let callback: ChannelsChangedCallback = Box::new(callback);
        self.engine.set_channels_changed_callback(callback);
    }

    /// The audio channels currently available in the session.
    pub fn channels(&self) -> Vec<Channel> {
        self.engine.channels()
    }

    /// Publishes a new audio channel to the session.
    ///
    /// `max_num_samples` should account for the number of channels times the
    /// number of frames written in one audio callback.
    pub fn add_sink(&self, name: impl Into<String>, max_num_samples: usize) -> LinkAudioSink {
        LinkAudioSink {
            sink: self.engine.add_sink(name, max_num_samples),
            engine: self.engine.clone(),
        }
    }

    /// Subscribes to a channel published by another peer. The callback is
    /// invoked on a Link-managed thread whenever a buffer arrives.
    pub fn add_source<F>(&self, id: Id, callback: F) -> LinkAudioSource
    where
        F: FnMut(SourceBufferHandle<'_>) + Send + 'static,
    {
        let mut callback = callback;
        LinkAudioSource {
            source: self.engine.add_source(
                id,
                Box::new(move |handle| {
                    callback(SourceBufferHandle {
                        samples: handle.samples,
                        info: handle.info,
                    })
                }),
            ),
            engine: self.engine.clone(),
        }
    }

    /// The endpoint LinkAudio traffic is received on.
    pub fn audio_endpoint(&self) -> SocketAddrV4 {
        self.engine.endpoint()
    }

    fn spawn_peer_sync(&self) -> tokio::task::JoinHandle<()> {
        let engine = self.engine.clone();
        let peers = self.link.controller().peers();
        let controller_peer_state = self.link.controller().peer_state.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PEER_SYNC_PERIOD);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;

                if let Ok(peer_state) = controller_peer_state.try_lock() {
                    engine.set_identity(peer_state.ident(), peer_state.session_id());
                }

                let discovered: Vec<(crate::link::node::NodeId, Option<SocketAddrV4>)> =
                    match peers.try_lock() {
                        Ok(peers) => peers
                            .iter()
                            .map(|peer| (peer.peer_state.ident(), peer.peer_state.audio_endpoint))
                            .collect(),
                        Err(_) => continue,
                    };

                for (peer_id, endpoint) in &discovered {
                    engine.saw_link_audio_endpoint(*peer_id, *endpoint);
                }

                let peer_ids: Vec<crate::link::node::NodeId> =
                    discovered.iter().map(|(id, _)| *id).collect();
                engine.update_session_peers(&peer_ids);
            }
        })
    }
}

impl Deref for LinkAudio {
    type Target = BasicLink;

    fn deref(&self) -> &Self::Target {
        &self.link
    }
}

impl DerefMut for LinkAudio {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.link
    }
}

impl Drop for LinkAudio {
    fn drop(&mut self) {
        if let Some(task) = self.sync_task.take() {
            task.abort();
        }
    }
}

/// A channel published to the Link session.
///
/// The channel is visible to other peers for the lifetime of the sink. Audio
/// is only sent while at least one peer has a matching source.
pub struct LinkAudioSink {
    sink: Arc<Sink>,
    engine: Arc<AudioEngine>,
}

impl LinkAudioSink {
    /// The channel identifier, which peers use to subscribe.
    pub fn id(&self) -> Id {
        self.sink.id()
    }

    pub fn name(&self) -> String {
        self.sink.name()
    }

    /// Renames the channel. Names longer than 256 bytes are truncated.
    pub fn set_name(&self, name: impl Into<String>) {
        self.sink.set_name(name);
    }

    /// Requests a larger buffer size for future buffers. Shrinking is a no-op.
    pub fn request_max_num_samples(&self, num_samples: usize) {
        self.sink.request_max_num_samples(num_samples);
    }

    pub fn max_num_samples(&self) -> usize {
        self.sink.max_num_samples()
    }

    /// Is any peer currently receiving this channel?
    pub fn is_connected(&self) -> bool {
        self.sink.is_connected()
    }

    /// Retains a buffer for writing. Returns `None` if no peer is listening or
    /// no buffer is available. Realtime-safe.
    pub fn buffer(&self) -> Option<SinkBufferHandle<'_>> {
        self.sink.retain_buffer()
    }
}

impl Drop for LinkAudioSink {
    fn drop(&mut self) {
        self.engine.remove_sink(self.sink.id());
    }
}

/// A subscription to a channel published by another peer.
pub struct LinkAudioSource {
    source: Arc<Source>,
    engine: Arc<AudioEngine>,
}

impl LinkAudioSource {
    /// The identifier of the channel being received.
    pub fn id(&self) -> Id {
        self.source.id()
    }
}

impl Drop for LinkAudioSource {
    fn drop(&mut self) {
        self.engine.remove_source(self.source.id());
    }
}

/// A block of received audio and the information needed to place it on the
/// local beat grid.
#[derive(Debug)]
pub struct SourceBufferHandle<'a> {
    /// The received samples, interleaved by channel.
    pub samples: &'a [i16],
    pub info: BufferInfo,
}

impl SourceBufferHandle<'_> {
    /// The local beat time at the beginning of the buffer, or `None` if the
    /// buffer originates from a different Link session.
    pub fn begin_beats(&self, session_state: &SessionState, quantum: f64) -> Option<f64> {
        begin_beats(&self.info, session_state, quantum)
    }

    /// The local beat time at the end of the buffer, or `None` if the buffer
    /// originates from a different Link session.
    pub fn end_beats(&self, session_state: &SessionState, quantum: f64) -> Option<f64> {
        end_beats(&self.info, session_state, quantum)
    }
}

impl BufferInfo {
    /// The local beat time at the beginning of the buffer.
    pub fn begin_beats(&self, session_state: &SessionState, quantum: f64) -> Option<f64> {
        begin_beats(self, session_state, quantum)
    }

    /// The local beat time at the end of the buffer.
    pub fn end_beats(&self, session_state: &SessionState, quantum: f64) -> Option<f64> {
        end_beats(self, session_state, quantum)
    }

    /// Duration of the buffer in beats, derived from its frame count, sample
    /// rate and tempo.
    pub fn duration_in_beats(&self) -> f64 {
        if self.sample_rate == 0 || self.tempo <= 0.0 {
            return 0.0;
        }
        let seconds = self.num_frames as f64 / self.sample_rate as f64;
        seconds * self.tempo / 60.0
    }
}

fn begin_beats(info: &BufferInfo, session_state: &SessionState, quantum: f64) -> Option<f64> {
    let timeline = session_state.timeline();
    Some(
        beat_at_global_beat(
            &timeline,
            Beats::new(info.session_beat_time),
            Beats::new(quantum),
        )
        .floating(),
    )
}

fn end_beats(info: &BufferInfo, session_state: &SessionState, quantum: f64) -> Option<f64> {
    Some(begin_beats(info, session_state, quantum)? + info.duration_in_beats())
}

/// Commits a sink buffer using a captured Link session state.
impl SinkBufferHandle<'_> {
    /// Commits the buffer, aligning it to the session beat grid.
    ///
    /// The session state, quantum and beats at buffer begin must be the same
    /// values used to render the audio locally.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_with_session_state(
        self,
        session_state: &SessionState,
        session_id: Id,
        beats_at_buffer_begin: f64,
        quantum: f64,
        num_frames: usize,
        num_channels: usize,
        sample_rate: u32,
    ) -> bool {
        let timeline = session_state.timeline();
        self.commit(
            &timeline,
            session_id,
            beats_at_buffer_begin,
            quantum,
            num_frames,
            num_channels,
            sample_rate,
        )
    }
}

fn local_ipv4() -> std::io::Result<Ipv4Addr> {
    list_afinet_netifas()
        .map_err(|e| std::io::Error::other(format!("failed to enumerate network interfaces: {e}")))?
        .iter()
        .find_map(|(_, ip)| match ip {
            IpAddr::V4(ipv4) if !ip.is_loopback() => Some(*ipv4),
            _ => None,
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "no non-loopback IPv4 interface found",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{
        node::NodeId,
        sessions::SessionId,
        state::{ClientStartStopState, ClientState},
        tempo::Tempo,
        timeline::Timeline,
        to_session_state,
    };
    use chrono::Duration;

    fn session_state() -> SessionState {
        to_session_state(
            &ClientState {
                timeline: Timeline {
                    tempo: Tempo::new(120.0),
                    beat_origin: Beats::new(0.0),
                    time_origin: Duration::zero(),
                },
                timeline_session_id: SessionId::default(),
                start_stop_state: ClientStartStopState {
                    is_playing: false,
                    time: Duration::zero(),
                    timestamp: Duration::zero(),
                },
            },
            false,
        )
    }

    fn info() -> BufferInfo {
        BufferInfo {
            num_channels: 2,
            num_frames: 22050,
            sample_rate: 44100,
            count: 1,
            session_beat_time: 4.0,
            tempo: 120.0,
            session_id: NodeId::default(),
        }
    }

    #[test]
    fn buffer_duration_is_derived_from_tempo_and_sample_rate() {
        // Half a second at 120 bpm is one beat.
        assert!((info().duration_in_beats() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zero_sample_rate_has_no_duration() {
        let info = BufferInfo {
            sample_rate: 0,
            ..info()
        };
        assert_eq!(info.duration_in_beats(), 0.0);
    }

    #[test]
    fn end_beats_follow_begin_beats() {
        let state = session_state();
        let info = info();
        let begin = info.begin_beats(&state, 4.0).unwrap();
        let end = info.end_beats(&state, 4.0).unwrap();
        assert!((end - begin - 1.0).abs() < 1e-9);
    }

    #[test]
    fn beat_mapping_roundtrips_through_the_session_state() {
        let state = session_state();
        let timeline = state.timeline();
        let local = crate::link_audio::beat_time_mapping::global_beat_at_beat(
            &timeline,
            Beats::new(4.0),
            Beats::new(4.0),
        );
        let info = BufferInfo {
            session_beat_time: local.floating(),
            ..info()
        };
        assert!((info.begin_beats(&state, 4.0).unwrap() - 4.0).abs() < 1e-9);
    }
}
