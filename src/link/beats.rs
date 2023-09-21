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

    pub fn size_in_byte_stream(self, beats: Beats) -> u32 {
        todo!()
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
