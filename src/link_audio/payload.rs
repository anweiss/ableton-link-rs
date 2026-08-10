//! Payload entries for the LinkAudio protocol.
//!
//! LinkAudio reuses the Link payload framing (a four character `key` followed
//! by a `u32` body size), but defines its own set of entries. Ported from
//! `ableton/link_audio/{PeerInfo,ChannelId,ChannelAnnouncements,ChannelRequests,
//! AudioBuffer,PeerAnnouncement}.hpp`.

use chrono::Duration;

use crate::link::{beats::Beats, node::NodeId, sessions::SessionId, tempo::Tempo};

use super::{
    encoding::{size_of_string, ByteStreamReader, ByteStreamWrite},
    error::{AudioError, Result},
    messages::{MAX_NAME_SIZE, MAX_PAYLOAD_SIZE},
};

/// Size of a payload entry header (key + size).
pub const ENTRY_HEADER_SIZE: u32 = 8;

pub const PEER_INFO_KEY: u32 = u32::from_be_bytes(*b"__pi");
pub const CHANNEL_ID_KEY: u32 = u32::from_be_bytes(*b"chid");
pub const CHANNEL_ANNOUNCEMENTS_KEY: u32 = u32::from_be_bytes(*b"auca");
pub const CHANNEL_BYES_KEY: u32 = u32::from_be_bytes(*b"aucb");
pub const AUDIO_BUFFER_KEY: u32 = u32::from_be_bytes(*b"_abu");
pub const SESSION_MEMBERSHIP_KEY: u32 = u32::from_be_bytes(*b"sess");
pub const HOST_TIME_KEY: u32 = u32::from_be_bytes(*b"__ht");

/// A value that can be carried as a LinkAudio payload entry.
pub trait Entry: Sized {
    const KEY: u32;

    /// Size of the encoded body, excluding the entry header.
    fn body_size(&self) -> u32;

    fn encode_body(&self, out: &mut Vec<u8>);

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self>;

    /// Size of the entry including its header.
    fn size_in_byte_stream(&self) -> u32 {
        ENTRY_HEADER_SIZE + self.body_size()
    }

    /// Appends the entry (header and body) to `out`.
    fn encode(&self, out: &mut Vec<u8>) {
        out.write_u32(Self::KEY);
        out.write_u32(self.body_size());
        self.encode_body(out);
    }

    /// Encodes the entry into a standalone payload buffer.
    fn to_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.size_in_byte_stream() as usize);
        self.encode(&mut out);
        out
    }
}

/// Walks the entries of a payload, invoking `visit` with the key and a reader
/// positioned at the start of the entry body. Unknown entries are skipped,
/// matching upstream's forward-compatible parsing.
pub fn parse_payload<F>(data: &[u8], mut visit: F) -> Result<()>
where
    F: FnMut(u32, &mut ByteStreamReader<'_>) -> Result<()>,
{
    let mut reader = ByteStreamReader::new(data);
    while reader.remaining() >= ENTRY_HEADER_SIZE as usize {
        let key = reader.read_u32()?;
        let size = reader.read_u32()? as usize;
        let body = reader.read_bytes(size)?;
        let mut body_reader = ByteStreamReader::new(body);
        visit(key, &mut body_reader)?;
    }
    Ok(())
}

/// Truncates a name to the maximum size allowed on the wire, respecting UTF-8
/// character boundaries.
pub fn truncate_name(name: &str) -> String {
    if name.len() <= MAX_NAME_SIZE {
        return name.to_string();
    }
    let mut end = MAX_NAME_SIZE;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// The identifier type shared by peers, sessions and channels.
pub type Id = NodeId;

fn write_id(out: &mut Vec<u8>, id: &Id) {
    out.extend_from_slice(&id.0);
}

fn read_id(reader: &mut ByteStreamReader<'_>) -> Result<Id> {
    Ok(NodeId(reader.read_array::<8>()?))
}

/// `__pi` — the display name of a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerInfo {
    pub name: String,
}

impl PeerInfo {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: truncate_name(&name.into()),
        }
    }
}

impl Entry for PeerInfo {
    const KEY: u32 = PEER_INFO_KEY;

    fn body_size(&self) -> u32 {
        size_of_string(&self.name)
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.write_string(&self.name);
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(PeerInfo {
            name: reader.read_string()?,
        })
    }
}

/// `chid` — the identifier of a channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelId {
    pub id: Id,
}

impl Entry for ChannelId {
    const KEY: u32 = CHANNEL_ID_KEY;

    fn body_size(&self) -> u32 {
        8
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        write_id(out, &self.id);
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(ChannelId {
            id: read_id(reader)?,
        })
    }
}

/// `sess` — the session a peer belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionMembership {
    pub session_id: SessionId,
}

impl Entry for SessionMembership {
    const KEY: u32 = SESSION_MEMBERSHIP_KEY;

    fn body_size(&self) -> u32 {
        8
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        write_id(out, &self.session_id.0);
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(SessionMembership {
            session_id: SessionId(read_id(reader)?),
        })
    }
}

/// `__ht` — a host time used for the ping/pong network quality measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTime {
    pub time: Duration,
}

impl Default for HostTime {
    fn default() -> Self {
        Self {
            time: Duration::zero(),
        }
    }
}

impl Entry for HostTime {
    const KEY: u32 = HOST_TIME_KEY;

    fn body_size(&self) -> u32 {
        8
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.write_i64(self.time.num_microseconds().unwrap_or(0));
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(HostTime {
            time: Duration::microseconds(reader.read_i64()?),
        })
    }
}

/// A single channel offered by a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelAnnouncement {
    pub name: String,
    pub id: Id,
}

impl ChannelAnnouncement {
    pub fn size_in_byte_stream(&self) -> u32 {
        size_of_string(&self.name) + 8
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.write_string(&self.name);
        write_id(out, &self.id);
    }

    fn decode(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        let name = reader.read_string()?;
        let id = read_id(reader)?;
        Ok(ChannelAnnouncement { name, id })
    }
}

/// `auca` — the set of channels a peer currently offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelAnnouncements {
    pub channels: Vec<ChannelAnnouncement>,
}

impl Entry for ChannelAnnouncements {
    const KEY: u32 = CHANNEL_ANNOUNCEMENTS_KEY;

    fn body_size(&self) -> u32 {
        4 + self
            .channels
            .iter()
            .map(|c| c.size_in_byte_stream())
            .sum::<u32>()
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.write_u32(self.channels.len() as u32);
        for channel in &self.channels {
            channel.encode(out);
        }
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(ChannelAnnouncements {
            channels: reader.read_vec(ChannelAnnouncement::decode)?,
        })
    }
}

/// A channel that is no longer offered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelBye {
    pub id: Id,
}

impl ChannelBye {
    pub fn size_in_byte_stream(&self) -> u32 {
        8
    }
}

/// `aucb` — channels that have gone away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelByes {
    pub byes: Vec<ChannelBye>,
}

impl Entry for ChannelByes {
    const KEY: u32 = CHANNEL_BYES_KEY;

    fn body_size(&self) -> u32 {
        4 + self
            .byes
            .iter()
            .map(|b| b.size_in_byte_stream())
            .sum::<u32>()
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.write_u32(self.byes.len() as u32);
        for bye in &self.byes {
            write_id(out, &bye.id);
        }
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        Ok(ChannelByes {
            byes: reader.read_vec(|r| Ok(ChannelBye { id: read_id(r)? }))?,
        })
    }
}

/// The audio codec used for an [`AudioBuffer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Codec {
    #[default]
    Invalid,
    /// Interleaved 16-bit signed PCM.
    PcmI16,
}

impl Codec {
    pub fn to_u8(self) -> u8 {
        match self {
            Codec::Invalid => 0,
            Codec::PcmI16 => 1,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Codec::PcmI16,
            _ => Codec::Invalid,
        }
    }
}

/// A contiguous run of frames within an [`AudioBuffer`] that shares one tempo
/// and beat origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunk {
    pub count: u64,
    pub num_frames: u16,
    pub begin_beats: Beats,
    pub tempo: Tempo,
}

impl Default for Chunk {
    fn default() -> Self {
        Self {
            count: 0,
            num_frames: 0,
            begin_beats: Beats::new(0.0),
            tempo: Tempo::new(0.0),
        }
    }
}

impl Chunk {
    pub const SIZE: u32 = 8 + 2 + 8 + 8;

    fn encode(&self, out: &mut Vec<u8>) {
        out.write_u64(self.count);
        out.write_u16(self.num_frames);
        out.write_i64(self.begin_beats.micro_beats());
        out.write_i64(
            self.tempo
                .micros_per_beat()
                .num_microseconds()
                .unwrap_or(i64::MAX),
        );
    }

    fn decode(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        let count = reader.read_u64()?;
        let num_frames = reader.read_u16()?;
        let begin_beats = Beats::from_microbeats(reader.read_i64()?);
        let micros_per_beat = reader.read_i64()?;
        let tempo = if micros_per_beat == 0 {
            Tempo::new(0.0)
        } else {
            Tempo::from(Duration::microseconds(micros_per_beat))
        };
        Ok(Chunk {
            count,
            num_frames,
            begin_beats,
            tempo,
        })
    }
}

/// `_abu` — a block of encoded audio for one channel.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBuffer {
    pub channel_id: Id,
    pub session_id: Id,
    pub chunks: Vec<Chunk>,
    pub codec: Codec,
    pub sample_rate: u32,
    pub num_channels: u8,
    pub bytes: Vec<u8>,
}

impl AudioBuffer {
    /// Bytes of an audio buffer message that are not audio data. Matches
    /// upstream's `kNonAudioBytes`.
    pub const NON_AUDIO_BYTES: usize = 50;
    /// Maximum number of encoded audio bytes that fit in one message.
    pub const MAX_AUDIO_BYTES: usize = MAX_PAYLOAD_SIZE - Self::NON_AUDIO_BYTES;

    pub fn num_frames(&self) -> u32 {
        self.chunks.iter().map(|c| c.num_frames as u32).sum()
    }
}

impl Default for AudioBuffer {
    fn default() -> Self {
        Self {
            channel_id: Id::default(),
            session_id: Id::default(),
            chunks: Vec::new(),
            codec: Codec::Invalid,
            sample_rate: 0,
            num_channels: 0,
            bytes: Vec::new(),
        }
    }
}

impl Entry for AudioBuffer {
    const KEY: u32 = AUDIO_BUFFER_KEY;

    fn body_size(&self) -> u32 {
        8 // channel id
            + 8 // session id
            + 4 + self.chunks.len() as u32 * Chunk::SIZE
            + 1 // codec
            + 4 // sample rate
            + 1 // num channels
            + 2 // num bytes
            + self.bytes.len() as u32
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        write_id(out, &self.channel_id);
        write_id(out, &self.session_id);
        out.write_u32(self.chunks.len() as u32);
        for chunk in &self.chunks {
            chunk.encode(out);
        }
        out.write_u8(self.codec.to_u8());
        out.write_u32(self.sample_rate);
        out.write_u8(self.num_channels);
        out.write_u16(self.bytes.len() as u16);
        out.extend_from_slice(&self.bytes);
    }

    fn decode_body(reader: &mut ByteStreamReader<'_>) -> Result<Self> {
        let channel_id = read_id(reader)?;
        let session_id = read_id(reader)?;
        let chunks = reader.read_vec(Chunk::decode)?;

        if chunks.is_empty() {
            return Err(AudioError::Invalid("audio buffer has no chunks"));
        }

        let codec = Codec::from_u8(reader.read_u8()?);
        if codec == Codec::Invalid {
            return Err(AudioError::Invalid("invalid codec"));
        }

        let sample_rate = reader.read_u32()?;
        let num_channels = reader.read_u8()?;
        let num_bytes = reader.read_u16()? as usize;

        let buffer = AudioBuffer {
            channel_id,
            session_id,
            chunks,
            codec,
            sample_rate,
            num_channels,
            bytes: Vec::new(),
        };

        if codec == Codec::PcmI16
            && buffer.num_frames() as usize * num_channels as usize * 2 != num_bytes
        {
            return Err(AudioError::Invalid("byte count / frame count mismatch"));
        }

        let bytes = reader.read_bytes(num_bytes)?.to_vec();

        Ok(AudioBuffer { bytes, ..buffer })
    }
}

/// A peer's announcement of itself and the channels it offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerAnnouncement {
    pub node_id: NodeId,
    pub session_id: SessionId,
    pub peer_info: PeerInfo,
    pub channels: ChannelAnnouncements,
}

impl PeerAnnouncement {
    pub fn ident(&self) -> NodeId {
        self.node_id
    }

    pub fn payload_size(&self) -> u32 {
        SessionMembership {
            session_id: self.session_id,
        }
        .size_in_byte_stream()
            + self.peer_info.size_in_byte_stream()
            + self.channels.size_in_byte_stream()
    }

    pub fn to_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload_size() as usize);
        SessionMembership {
            session_id: self.session_id,
        }
        .encode(&mut out);
        self.peer_info.encode(&mut out);
        self.channels.encode(&mut out);
        out
    }

    pub fn from_payload(node_id: NodeId, data: &[u8]) -> Result<Self> {
        let mut announcement = PeerAnnouncement {
            node_id,
            ..Default::default()
        };

        parse_payload(data, |key, reader| {
            match key {
                SESSION_MEMBERSHIP_KEY => {
                    announcement.session_id = SessionMembership::decode_body(reader)?.session_id;
                }
                PEER_INFO_KEY => {
                    announcement.peer_info = PeerInfo::decode_body(reader)?;
                }
                CHANNEL_ANNOUNCEMENTS_KEY => {
                    announcement.channels = ChannelAnnouncements::decode_body(reader)?;
                }
                _ => {}
            }
            Ok(())
        })?;

        Ok(announcement)
    }
}

/// A request from a peer to start receiving a channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelRequest {
    pub peer_id: Id,
    pub channel_id: Id,
}

impl ChannelRequest {
    pub fn to_payload(&self) -> Vec<u8> {
        ChannelId {
            id: self.channel_id,
        }
        .to_payload()
    }

    pub fn from_payload(peer_id: Id, data: &[u8]) -> Result<Self> {
        let mut request = ChannelRequest {
            peer_id,
            ..Default::default()
        };
        parse_payload(data, |key, reader| {
            if key == CHANNEL_ID_KEY {
                request.channel_id = ChannelId::decode_body(reader)?.id;
            }
            Ok(())
        })?;
        Ok(request)
    }
}

/// A request from a peer to stop receiving a channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelStopRequest {
    pub peer_id: Id,
    pub channel_id: Id,
}

impl ChannelStopRequest {
    pub fn to_payload(&self) -> Vec<u8> {
        ChannelId {
            id: self.channel_id,
        }
        .to_payload()
    }

    pub fn from_payload(peer_id: Id, data: &[u8]) -> Result<Self> {
        let request = ChannelRequest::from_payload(peer_id, data)?;
        Ok(ChannelStopRequest {
            peer_id: request.peer_id,
            channel_id: request.channel_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Id {
        NodeId::from_array([n; 8])
    }

    #[test]
    fn keys_match_upstream_fourcc() {
        assert_eq!(PEER_INFO_KEY, 0x5f5f_7069);
        assert_eq!(CHANNEL_ID_KEY, 0x6368_6964);
        assert_eq!(CHANNEL_ANNOUNCEMENTS_KEY, 0x6175_6361);
        assert_eq!(CHANNEL_BYES_KEY, 0x6175_6362);
        assert_eq!(AUDIO_BUFFER_KEY, 0x5f61_6275);
    }

    #[test]
    fn peer_info_roundtrip() {
        let info = PeerInfo::new("rusthut");
        let encoded = info.to_payload();
        assert_eq!(encoded.len(), info.size_in_byte_stream() as usize);

        let mut decoded = PeerInfo::default();
        parse_payload(&encoded, |key, reader| {
            if key == PEER_INFO_KEY {
                decoded = PeerInfo::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn long_names_are_truncated() {
        let name = "a".repeat(MAX_NAME_SIZE + 100);
        assert_eq!(PeerInfo::new(name).name.len(), MAX_NAME_SIZE);
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        let name = "é".repeat(MAX_NAME_SIZE);
        let truncated = truncate_name(&name);
        assert!(truncated.len() <= MAX_NAME_SIZE);
        assert!(name.starts_with(&truncated));
    }

    #[test]
    fn channel_announcements_roundtrip() {
        let announcements = ChannelAnnouncements {
            channels: vec![
                ChannelAnnouncement {
                    name: "left".to_string(),
                    id: id(1),
                },
                ChannelAnnouncement {
                    name: "right".to_string(),
                    id: id(2),
                },
            ],
        };
        let encoded = announcements.to_payload();
        assert_eq!(encoded.len(), announcements.size_in_byte_stream() as usize);

        let mut decoded = ChannelAnnouncements::default();
        parse_payload(&encoded, |key, reader| {
            if key == CHANNEL_ANNOUNCEMENTS_KEY {
                decoded = ChannelAnnouncements::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded, announcements);
    }

    #[test]
    fn channel_byes_roundtrip() {
        let byes = ChannelByes {
            byes: vec![ChannelBye { id: id(7) }, ChannelBye { id: id(8) }],
        };
        let encoded = byes.to_payload();
        assert_eq!(encoded.len(), byes.size_in_byte_stream() as usize);

        let mut decoded = ChannelByes::default();
        parse_payload(&encoded, |key, reader| {
            if key == CHANNEL_BYES_KEY {
                decoded = ChannelByes::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded, byes);
    }

    #[test]
    fn peer_announcement_roundtrip() {
        let announcement = PeerAnnouncement {
            node_id: id(3),
            session_id: SessionId(id(4)),
            peer_info: PeerInfo::new("peer"),
            channels: ChannelAnnouncements {
                channels: vec![ChannelAnnouncement {
                    name: "main".to_string(),
                    id: id(5),
                }],
            },
        };

        let payload = announcement.to_payload();
        assert_eq!(payload.len(), announcement.payload_size() as usize);

        let decoded = PeerAnnouncement::from_payload(id(3), &payload).unwrap();
        assert_eq!(decoded, announcement);
    }

    #[test]
    fn channel_request_roundtrip() {
        let request = ChannelRequest {
            peer_id: id(1),
            channel_id: id(2),
        };
        let decoded = ChannelRequest::from_payload(id(1), &request.to_payload()).unwrap();
        assert_eq!(decoded, request);

        let stop = ChannelStopRequest {
            peer_id: id(1),
            channel_id: id(2),
        };
        let decoded = ChannelStopRequest::from_payload(id(1), &stop.to_payload()).unwrap();
        assert_eq!(decoded, stop);
    }

    #[test]
    fn audio_buffer_roundtrip() {
        let buffer = AudioBuffer {
            channel_id: id(9),
            session_id: id(10),
            chunks: vec![Chunk {
                count: 42,
                num_frames: 2,
                begin_beats: Beats::new(1.5),
                tempo: Tempo::new(120.0),
            }],
            codec: Codec::PcmI16,
            sample_rate: 44100,
            num_channels: 2,
            // 2 frames * 2 channels * 2 bytes
            bytes: vec![0, 1, 0, 2, 0, 3, 0, 4],
        };

        let encoded = buffer.to_payload();
        assert_eq!(encoded.len(), buffer.size_in_byte_stream() as usize);

        let mut decoded = AudioBuffer::default();
        parse_payload(&encoded, |key, reader| {
            if key == AUDIO_BUFFER_KEY {
                decoded = AudioBuffer::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(decoded.channel_id, buffer.channel_id);
        assert_eq!(decoded.session_id, buffer.session_id);
        assert_eq!(decoded.chunks.len(), 1);
        assert_eq!(decoded.chunks[0].count, 42);
        assert_eq!(decoded.chunks[0].num_frames, 2);
        assert_eq!(decoded.chunks[0].begin_beats, Beats::new(1.5));
        assert!((decoded.chunks[0].tempo.bpm() - 120.0).abs() < 1e-6);
        assert_eq!(decoded.bytes, buffer.bytes);
        assert_eq!(decoded.num_frames(), 2);
    }

    #[test]
    fn audio_buffer_rejects_frame_byte_mismatch() {
        let buffer = AudioBuffer {
            channel_id: id(1),
            session_id: id(2),
            chunks: vec![Chunk {
                count: 1,
                num_frames: 4,
                begin_beats: Beats::new(0.0),
                tempo: Tempo::new(120.0),
            }],
            codec: Codec::PcmI16,
            sample_rate: 44100,
            num_channels: 2,
            bytes: vec![0, 1],
        };

        let encoded = buffer.to_payload();
        let err = parse_payload(&encoded, |key, reader| {
            if key == AUDIO_BUFFER_KEY {
                AudioBuffer::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(matches!(err, AudioError::Invalid(_)));
    }

    #[test]
    fn unknown_entries_are_skipped() {
        let mut payload = Vec::new();
        payload.write_u32(u32::from_be_bytes(*b"zzzz"));
        payload.write_u32(4);
        payload.extend_from_slice(&[1, 2, 3, 4]);
        PeerInfo::new("after").encode(&mut payload);

        let mut name = String::new();
        parse_payload(&payload, |key, reader| {
            if key == PEER_INFO_KEY {
                name = PeerInfo::decode_body(reader)?.name;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(name, "after");
    }

    #[test]
    fn host_time_roundtrip() {
        let ht = HostTime {
            time: Duration::microseconds(1_234_567),
        };
        let encoded = ht.to_payload();
        let mut decoded = HostTime::default();
        parse_payload(&encoded, |key, reader| {
            if key == HOST_TIME_KEY {
                decoded = HostTime::decode_body(reader)?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(decoded, ht);
    }
}
