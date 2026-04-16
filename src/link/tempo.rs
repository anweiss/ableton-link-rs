use core::{
    fmt::{self, Display},
    mem,
};

use chrono::Duration;
#[cfg(not(feature = "std"))]
use num_traits::float::FloatCore;

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

impl crate::encoding::Encode for Tempo {
    fn encode_to(&self, out: &mut alloc::vec::Vec<u8>) {
        self.micros_per_beat()
            .num_microseconds()
            .unwrap()
            .encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl crate::encoding::Decode for Tempo {
    fn decode_from(
        bytes: &[u8],
    ) -> core::result::Result<(Self, usize), crate::encoding::DecodeError> {
        let (micros, n) = i64::decode_from(bytes)?;
        Ok((Self::from(Duration::microseconds(micros)), n))
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

    #[test]
    fn tempo_display() {
        let tempo = Tempo::new(128.5);
        assert_eq!(format!("{}", tempo), "128.5");
    }

    #[test]
    fn tempo_from_duration() {
        // 500,000 µs per beat → 120 BPM
        let tempo = Tempo::from(Duration::microseconds(500_000));
        assert_eq!(tempo.bpm(), 120.0);
    }

    #[test]
    fn tempo_micros_to_beats_zero_duration() {
        let tempo = Tempo::new(120.0);
        assert_eq!(tempo.micros_to_beats(Duration::zero()), Beats::new(0.0));
    }

    #[test]
    fn tempo_beats_to_micros_zero_beats() {
        let tempo = Tempo::new(120.0);
        assert_eq!(tempo.beats_to_micros(Beats::new(0.0)), Duration::zero());
    }

    #[test]
    fn tempo_roundtrip_bpm_to_micros_and_back() {
        for bpm in [20.0, 60.0, 120.0, 200.0, 999.0] {
            let tempo = Tempo::new(bpm);
            let micros = tempo.micros_per_beat();
            let recovered = Tempo::from(micros);
            assert!(
                (recovered.bpm() - bpm).abs() < 0.01,
                "roundtrip failed for bpm={}",
                bpm
            );
        }
    }

    #[test]
    fn tempo_very_slow() {
        let tempo = Tempo::new(20.0);
        // 20 BPM → 3,000,000 µs per beat
        assert_eq!(tempo.micros_per_beat(), Duration::microseconds(3_000_000));
    }

    #[test]
    fn tempo_very_fast() {
        let tempo = Tempo::new(999.0);
        let mpb = tempo.micros_per_beat();
        // 999 BPM → ~60060 µs per beat
        assert!(mpb.num_microseconds().unwrap() > 0);
        assert!(mpb.num_microseconds().unwrap() < 100_000);
    }

    #[test]
    fn tempo_equality() {
        assert_eq!(Tempo::new(120.0), Tempo { value: 120.0 });
        assert_ne!(Tempo::new(120.0), Tempo::new(121.0));
    }

    #[test]
    fn tempo_default_is_zero() {
        assert_eq!(Tempo::default().bpm(), 0.0);
    }
}
