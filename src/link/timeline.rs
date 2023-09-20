use std::{mem, time::Duration};

use bincode::{Decode, Encode};

use crate::discovery::{payload::PayloadEntryHeader, ENCODING_CONFIG};

use super::{beats::Beats, tempo::Tempo, Result};

pub const TIMELINE_HEADER_KEY: u32 = u32::from_be_bytes(*b"tmln");
pub const TIMELINE_SIZE: u32 =
    (mem::size_of::<Tempo>() + mem::size_of::<Beats>() + mem::size_of::<Duration>()) as u32;
pub const TIMELINE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: TIMELINE_HEADER_KEY,
    size: TIMELINE_SIZE,
};

#[derive(PartialEq, Encode, Decode, Clone, Default, Debug, Copy)]
pub struct Timeline {
    tempo: Tempo,
    beat_origin: Beats,
    time_origin: Duration,
}

impl Timeline {
    fn to_beats(&self, time: Duration) -> Beats {
        todo!()
    }

    fn from_beats(&self, beats: Beats) -> Duration {
        todo!()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = TIMELINE_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(&self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}
