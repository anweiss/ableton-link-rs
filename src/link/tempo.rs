use std::mem;

use bincode::{Decode, Encode};

pub const TEMPO_SIZE: u32 = mem::size_of::<f64>() as u32;

#[derive(Default, Debug, PartialEq, Encode, Decode, Clone, Copy)]
pub struct Tempo {
    pub value: f64,
}

impl Tempo {
    pub fn new(bpm: f64) -> Self {
        Tempo { value: bpm }
    }
}
