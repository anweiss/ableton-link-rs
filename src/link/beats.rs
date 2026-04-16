use core::{
    mem,
    ops::{Add, Neg, Sub},
};

use crate::encoding::{self, Decode, Encode};

pub const BEATS_SIZE: u32 = mem::size_of::<i64>() as u32;

#[derive(PartialEq, Eq, Copy, Clone, Default, PartialOrd, Ord, Debug)]
pub struct Beats {
    pub value: i64,
}

impl Encode for Beats {
    fn encode_to(&self, out: &mut alloc::vec::Vec<u8>) {
        self.micro_beats().encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl Decode for Beats {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), encoding::DecodeError> {
        let (value, n) = i64::decode_from(bytes)?;
        Ok((Self { value }, n))
    }
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

    #[test]
    fn beats_neg() {
        let b = Beats::new(3.0);
        assert_eq!((-b).floating(), -3.0);
    }

    #[test]
    fn beats_add() {
        let a = Beats::new(1.5);
        let b = Beats::new(2.5);
        assert_eq!((a + b).floating(), 4.0);
    }

    #[test]
    fn beats_sub() {
        let a = Beats::new(5.0);
        let b = Beats::new(2.0);
        assert_eq!((a - b).floating(), 3.0);
    }

    #[test]
    fn beats_abs() {
        assert_eq!(Beats::new(-3.0).abs(), Beats::new(3.0));
        assert_eq!(Beats::new(3.0).abs(), Beats::new(3.0));
        assert_eq!(Beats::new(0.0).abs(), Beats::new(0.0));
    }

    #[test]
    fn beats_mod() {
        let a = Beats::new(7.0);
        let b = Beats::new(4.0);
        assert_eq!(a.r#mod(b).floating(), 3.0);
    }

    #[test]
    fn beats_into_f64() {
        let b = Beats::new(2.5);
        let f: f64 = b.into();
        assert!((f - 2.5).abs() < 1e-10);
    }

    #[test]
    fn beats_default_is_zero() {
        let b = Beats::default();
        assert_eq!(b.value, 0);
        assert_eq!(b.floating(), 0.0);
    }

    #[test]
    fn beats_ordering() {
        assert!(Beats::new(1.0) < Beats::new(2.0));
        assert!(Beats::new(2.0) > Beats::new(1.0));
        assert_eq!(Beats::new(1.0), Beats::new(1.0));
    }

    #[test]
    fn beats_negative_value() {
        let b = Beats::new(-2.5);
        assert_eq!(b.micro_beats(), -2_500_000);
        assert!((b.floating() - (-2.5)).abs() < 1e-10);
    }
}
