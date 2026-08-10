//! PCM encoding and decoding for LinkAudio.
//!
//! Ported from `ableton/link_audio/PCMCodec.hpp` and
//! `ableton/link_audio/Encoder.hpp`. Audio is carried as interleaved 16-bit
//! signed samples in network byte order.

use crate::link::node::NodeId;

use super::{
    buffer::{Buffer, BufferCallbackHandle, BufferInfo},
    encoding::{ByteStreamReader, ByteStreamWrite},
    messages::HEADER_SIZE,
    payload::{AudioBuffer, Chunk, Codec, Id},
    resizer::{Resizer, ResizerSink},
};

/// Upstream sizes audio buffer messages against RFC 791, which requires nodes
/// to process IP messages of at least 576 bytes.
pub const MAX_AUDIO_BYTES: usize = 576 - HEADER_SIZE - AudioBuffer::NON_AUDIO_BYTES;

const _: () = assert!(MAX_AUDIO_BYTES <= AudioBuffer::MAX_AUDIO_BYTES);

/// Receives fully formed audio buffer messages ready to be sent.
pub trait AudioBufferSender {
    fn send(&mut self, buffer: &AudioBuffer);
}

impl<F> AudioBufferSender for F
where
    F: FnMut(&AudioBuffer),
{
    fn send(&mut self, buffer: &AudioBuffer) {
        self(buffer)
    }
}

/// Serializes chunks of interleaved `i16` samples into [`AudioBuffer`]s.
pub struct PcmEncoder<S> {
    output: AudioBuffer,
    sender: S,
}

impl<S: AudioBufferSender> PcmEncoder<S> {
    pub fn new(sender: S, channel_id: Id) -> Self {
        PcmEncoder {
            output: AudioBuffer {
                channel_id,
                ..Default::default()
            },
            sender,
        }
    }
}

impl<S: AudioBufferSender> ResizerSink for PcmEncoder<S> {
    fn flush(
        &mut self,
        samples: &[i16],
        chunks: &[Chunk],
        num_channels: u32,
        sample_rate: u32,
        session_id: Id,
    ) {
        self.output.chunks = chunks.to_vec();
        self.output.codec = Codec::PcmI16;
        self.output.sample_rate = sample_rate;
        self.output.num_channels = num_channels as u8;
        self.output.session_id = session_id;

        let num_samples = self.output.num_frames() as usize * num_channels as usize;
        let num_samples = num_samples.min(samples.len());

        self.output.bytes.clear();
        self.output
            .bytes
            .reserve(num_samples * core::mem::size_of::<i16>());
        for sample in &samples[..num_samples] {
            self.output.bytes.write_i16(*sample);
        }

        // Truncate the trailing chunk metadata if the sample buffer was short,
        // so that the declared frame count always matches the payload.
        if self.output.num_frames() as usize * num_channels as usize != num_samples {
            return;
        }

        self.sender.send(&self.output);
    }
}

/// Splits application buffers into message-sized [`AudioBuffer`]s.
pub struct Encoder<S> {
    resizer: Resizer<PcmEncoder<S>>,
}

impl<S: AudioBufferSender> Encoder<S> {
    pub fn new(sender: S, channel_id: Id) -> Self {
        Encoder {
            resizer: Resizer::new(PcmEncoder::new(sender, channel_id), MAX_AUDIO_BYTES),
        }
    }

    /// Encodes and sends the contents of `input`.
    pub fn encode(&mut self, input: &Buffer) {
        self.resizer.process(
            &input.samples,
            input.num_frames,
            input.num_channels,
            input.sample_rate,
            input.begin_beats,
            input.tempo,
            input.session_id,
        );
    }

    /// Sends any audio that has been cached but not yet transmitted.
    pub fn flush(&mut self) {
        self.resizer.flush();
    }
}

/// Decodes received [`AudioBuffer`]s into per-chunk sample blocks.
pub struct PcmDecoder {
    samples: Vec<i16>,
}

impl PcmDecoder {
    pub fn new(cache_size: usize) -> Self {
        PcmDecoder {
            samples: vec![0; cache_size],
        }
    }

    /// Decodes `input`, invoking `callback` once per chunk.
    pub fn decode<F>(&mut self, input: &AudioBuffer, mut callback: F)
    where
        F: FnMut(BufferCallbackHandle<'_>),
    {
        if input.codec != Codec::PcmI16 || input.num_channels == 0 {
            return;
        }

        let mut reader = ByteStreamReader::new(&input.bytes);
        let mut num_samples = 0usize;
        while !reader.is_empty() {
            match reader.read_i16() {
                Ok(sample) => {
                    if num_samples == self.samples.len() {
                        self.samples.push(sample);
                    } else {
                        self.samples[num_samples] = sample;
                    }
                    num_samples += 1;
                }
                Err(_) => break,
            }
        }

        let num_channels = input.num_channels as usize;
        let mut offset = 0usize;

        for chunk in &input.chunks {
            let chunk_samples = chunk.num_frames as usize * num_channels;
            if offset + chunk_samples > num_samples {
                break;
            }

            callback(BufferCallbackHandle {
                samples: &self.samples[offset..offset + chunk_samples],
                info: BufferInfo {
                    num_channels,
                    num_frames: chunk.num_frames as usize,
                    sample_rate: input.sample_rate,
                    count: chunk.count,
                    session_beat_time: chunk.begin_beats.floating(),
                    tempo: chunk.tempo.bpm(),
                    session_id: input.session_id,
                },
            });

            offset += chunk_samples;
        }
    }
}

impl Default for PcmDecoder {
    fn default() -> Self {
        Self::new(4096)
    }
}

/// The session identifier type carried by audio buffers.
pub type SessionIdBytes = NodeId;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{beats::Beats, tempo::Tempo};
    use std::sync::{Arc, Mutex};

    fn id(n: u8) -> Id {
        NodeId::from_array([n; 8])
    }

    #[test]
    fn encode_decode_roundtrip() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = sent.clone();
        let mut encoder = Encoder::new(
            move |buffer: &AudioBuffer| sink.lock().unwrap().push(buffer.clone()),
            id(1),
        );

        let samples: Vec<i16> = (0..64).map(|i| i as i16 * 100).collect();
        let mut buffer = Buffer::new(samples.len());
        buffer.samples = samples.clone();
        buffer.num_frames = 32;
        buffer.num_channels = 2;
        buffer.sample_rate = 44100;
        buffer.begin_beats = Beats::new(2.0);
        buffer.tempo = Tempo::new(120.0);
        buffer.session_id = id(2);

        encoder.encode(&buffer);
        encoder.flush();

        let sent = sent.lock().unwrap();
        assert!(!sent.is_empty());

        let mut decoder = PcmDecoder::new(4096);
        let mut decoded = Vec::new();
        for audio_buffer in sent.iter() {
            assert_eq!(audio_buffer.channel_id, id(1));
            assert_eq!(audio_buffer.session_id, id(2));
            assert_eq!(audio_buffer.codec, Codec::PcmI16);
            decoder.decode(audio_buffer, |handle| {
                assert_eq!(handle.info.num_channels, 2);
                assert_eq!(handle.info.sample_rate, 44100);
                assert!((handle.info.tempo - 120.0).abs() < 1e-6);
                decoded.extend_from_slice(handle.samples);
            });
        }

        assert_eq!(decoded, samples);
    }

    #[test]
    fn wire_roundtrip_preserves_samples() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = sent.clone();
        let mut encoder = Encoder::new(
            move |buffer: &AudioBuffer| {
                use super::super::payload::Entry;
                sink.lock().unwrap().push(buffer.to_payload())
            },
            id(3),
        );

        let samples: Vec<i16> = (0..8).map(|i| -i as i16).collect();
        let mut buffer = Buffer::new(samples.len());
        buffer.samples = samples.clone();
        buffer.num_frames = 4;
        buffer.num_channels = 2;
        buffer.sample_rate = 48000;
        buffer.begin_beats = Beats::new(0.5);
        buffer.tempo = Tempo::new(128.0);
        buffer.session_id = id(4);

        encoder.encode(&buffer);
        encoder.flush();

        let mut decoder = PcmDecoder::new(64);
        let mut decoded = Vec::new();
        for payload in sent.lock().unwrap().iter() {
            use super::super::payload::{parse_payload, Entry, AUDIO_BUFFER_KEY};
            parse_payload(payload, |key, reader| {
                if key == AUDIO_BUFFER_KEY {
                    let audio_buffer = AudioBuffer::decode_body(reader)?;
                    decoder.decode(&audio_buffer, |handle| {
                        decoded.extend_from_slice(handle.samples);
                    });
                }
                Ok(())
            })
            .unwrap();
        }

        assert_eq!(decoded, samples);
    }

    #[test]
    fn invalid_codec_decodes_to_nothing() {
        let mut decoder = PcmDecoder::new(16);
        let buffer = AudioBuffer {
            codec: Codec::Invalid,
            num_channels: 2,
            ..Default::default()
        };
        let mut called = false;
        decoder.decode(&buffer, |_| called = true);
        assert!(!called);
    }

    #[test]
    fn max_audio_bytes_fits_the_payload_budget() {
        const { assert!(MAX_AUDIO_BYTES <= AudioBuffer::MAX_AUDIO_BYTES) };
        assert_eq!(MAX_AUDIO_BYTES, 576 - HEADER_SIZE - 50);
    }
}
