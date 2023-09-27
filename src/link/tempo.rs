use std::{
    fmt::{self, Display},
    mem,
    time::Duration,
};

use super::beats::Beats;

pub const TEMPO_SIZE: u32 = mem::size_of::<f64>() as u32;

#[derive(Default, Debug, PartialEq, Clone, Copy)]
pub struct Tempo {
    pub value: f64,
}

impl Display for Tempo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:.1}", self.value)
    }
}

impl From<Duration> for Tempo {
    fn from(value: Duration) -> Self {
        Tempo {
            value: 60.0 * 1e6 / value.as_micros() as f64,
        }
    }
}

impl Tempo {
    pub fn new(bpm: f64) -> Self {
        Tempo { value: bpm }
    }

    pub fn bpm(&self) -> f64 {
        self.value
    }

    pub fn micros_per_beat(&self) -> Duration {
        Duration::from_micros((60.0 * 1e6 / self.bpm()).round() as u64)
    }

    pub fn micros_to_beats(&self, micros: u64) -> Beats {
        Beats::new(micros as f64 / self.micros_per_beat().as_micros() as f64)
    }

    pub fn beats_to_micros(&self, beats: Beats) -> Duration {
        Duration::from_micros(
            (beats.floating() * self.micros_per_beat().as_micros() as f64).round() as u64,
        )
    }
}

impl bincode::Encode for Tempo {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(&self.micros_per_beat().as_micros(), encoder)
    }
}

impl bincode::de::Decode for Tempo {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        Ok(Self::from(Duration::from_micros(bincode::Decode::decode(
            decoder,
        )?)))
    }
}
