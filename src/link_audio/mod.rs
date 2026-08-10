//! LinkAudio — streaming audio between Link peers.
//!
//! This module is a port of the upstream `ableton/LinkAudio.hpp` subsystem. It
//! is gated behind the optional `audio` cargo feature and layers a second,
//! independent UDP protocol on top of Link Classic: peers advertise a LinkAudio
//! endpoint in their Link `PeerState` (the `aep4` payload entry), then exchange
//! channel announcements and PCM audio buffers directly over that endpoint.
//!
//! The port contains no `unsafe` code.
//!
//! # Layers
//!
//! * [`encoding`] — the byte stream primitives used by the LinkAudio wire
//!   format (`u32`-prefixed strings and vectors, big endian scalars).
//! * [`messages`] — the v1 message framing: protocol header, message header
//!   and message types.
//! * [`payload`] — the payload entries (`__pi`, `chid`, `auca`, `aucb`,
//!   `_abu`, `sess`, `__ht`) and the messages assembled from them.
//! * [`buffer`], [`queue`], [`resizer`], [`codec`] — the realtime-safe audio
//!   pipeline: application buffers are handed over through a pre-allocated
//!   buffer pool, resized into message-sized chunks and PCM encoded.
//! * [`beat_time_mapping`], [`network_metrics`] — session-global beat time
//!   mapping and the link-quality filter used to choose a peer gateway.

// The LinkAudio port is entirely safe Rust.
#![forbid(unsafe_code)]

pub mod api;
pub mod beat_time_mapping;
pub mod buffer;
pub mod channels;
pub mod codec;
pub mod encoding;
pub mod engine;
pub mod error;
pub mod messages;
pub mod network_metrics;
pub mod payload;
pub mod queue;
pub mod receivers;
pub mod resizer;
pub mod sink;
pub mod source;

pub use api::{LinkAudio, LinkAudioSink, LinkAudioSource, SourceBufferHandle};
pub use buffer::{Buffer, BufferCallbackHandle, BufferInfo};
pub use channels::Channel;
pub use codec::{Encoder, PcmDecoder, MAX_AUDIO_BYTES};
pub use error::{AudioError, Result};
pub use payload::{AudioBuffer, Chunk, Codec, Id};
