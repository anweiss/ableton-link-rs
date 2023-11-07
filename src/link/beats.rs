use std::{
    mem,
    ops::{Add, Neg, Sub},
};

use bincode::{Decode, Encode};

pub const BEATS_SIZE: u32 = mem::size_of::<i64>() as u32;

#[derive(PartialEq, Eq, Copy, Clone, Default, PartialOrd, Encode, Decode, Debug)]
pub struct Beats {
    pub value: i64,
}

impl Beats {
    pub const SIZE: u32 = mem::size_of::<i64>() as u32;

    pub fn new(beats: f64) -> Self {
        Self {
            value: (beats * 1e6).round() as i64,
        }
    }

    pub fn from_microbeats(micro_beats: i64) -> Self {
        Self { value: micro_beats }
    }

    pub fn micro_beats(self) -> i64 {
        self.value
    }

    pub fn abs(self) -> Self {
        Self {
            value: self.value.abs(),
        }
    }

    pub fn r#mod(self, rhs: Beats) -> Self {
        Self {
            value: self.value % rhs.value,
        }
    }

    pub fn floating(self) -> f64 {
        self.value as f64 / 1e6
    }
}

impl From<Beats> for f64 {
    fn from(beats: Beats) -> Self {
        beats.value as f64 / 1e6
    }
}

impl Neg for Beats {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self { value: -self.value }
    }
}

impl Add for Beats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            value: self.value + rhs.value,
        }
    }
}

impl Sub for Beats {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self {
            value: self.value - rhs.value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_floating() {
        let beats = Beats::new(0.5);
        assert_eq!(beats.micro_beats(), 500000);
        assert!((0.5 - beats.floating()).abs() < 1e-10);
    }

    #[test]
    fn from_micros() {
        let beats = Beats::from_microbeats(100000);
        assert_eq!(beats.micro_beats(), 100000);
        assert!((0.1 - beats.floating()).abs() < 1e-10);
    }

    #[test]
    fn size_bytes() {
        assert_eq!(BEATS_SIZE, 8);
    }
}
