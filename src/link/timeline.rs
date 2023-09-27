use std::{mem, time::Duration};

use bincode::Encode;

use crate::discovery::{payload::PayloadEntryHeader, ENCODING_CONFIG};

use super::{
    beats::{Beats, BEATS_SIZE},
    tempo::{Tempo, TEMPO_SIZE},
    Result,
};

pub const TIMELINE_HEADER_KEY: u32 = u32::from_be_bytes(*b"tmln");
pub const TIMELINE_SIZE: u32 = TEMPO_SIZE + BEATS_SIZE + mem::size_of::<u64>() as u32;
pub const TIMELINE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: TIMELINE_HEADER_KEY,
    size: TIMELINE_SIZE,
};

#[derive(PartialEq, Clone, Default, Debug, Copy)]
pub struct Timeline {
    pub tempo: Tempo,
    pub beat_origin: Beats,
    pub time_origin: Duration,
}

impl Timeline {
    fn to_beats(&self, _time: Duration) -> Beats {
        todo!()
    }

    fn from_beats(&self, _beats: Beats) -> Duration {
        todo!()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = TIMELINE_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

impl bincode::Encode for Timeline {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(
            &(
                self.tempo,
                self.beat_origin,
                self.time_origin.as_micros() as u64,
            ),
            encoder,
        )
    }
}

impl bincode::de::Decode for Timeline {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (tempo, beat_origin, time_origin) = bincode::Decode::decode(decoder)?;
        Ok(Self {
            tempo,
            beat_origin,
            time_origin: Duration::from_micros(time_origin),
        })
    }
}
