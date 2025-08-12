use std::{
    fmt::{self, Display},
    mem,
};

use chrono::Duration;

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
            value: 60.0 * 1e6 / value.num_microseconds().unwrap() as f64,
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
        Duration::microseconds((60.0 * 1e6 / self.bpm()).round() as i64)
    }

    pub fn micros_to_beats(&self, micros: Duration) -> Beats {
        Beats::new(
            micros.num_microseconds().unwrap() as f64
                / self.micros_per_beat().num_microseconds().unwrap() as f64,
        )
    }

    pub fn beats_to_micros(&self, beats: Beats) -> Duration {
        Duration::microseconds(
            (beats.floating() * self.micros_per_beat().num_microseconds().unwrap() as f64).round()
                as i64,
        )
    }
}

impl bincode::Encode for Tempo {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(&self.micros_per_beat().num_microseconds().unwrap(), encoder)
    }
}

impl bincode::Decode<()> for Tempo {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        Ok(Self::from(Duration::microseconds(bincode::Decode::decode(
            decoder,
        )?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_from_bpm() {
        let tempo = Tempo::new(120.0);
        assert_eq!(tempo.micros_per_beat(), Duration::microseconds(500000));
    }

    #[test]
    fn micros_to_beats() {
        let tempo = Tempo::new(120.0);
        assert_eq!(
            tempo.micros_to_beats(Duration::microseconds(1000000)),
            Beats::new(2.0)
        );
    }

    #[test]
    fn beats_to_micros() {
        let tempo = Tempo::new(120.0);
        assert_eq!(
            tempo.beats_to_micros(Beats::new(2.0)),
            Duration::microseconds(1000000)
        );
    }
}
