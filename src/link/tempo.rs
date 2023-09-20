use bincode::{Decode, Encode};

#[derive(Default, Debug, PartialEq, Encode, Decode, Clone, Copy)]
pub struct Tempo {
    pub value: f64,
}

impl Tempo {
    pub fn new(bpm: f64) -> Self {
        Tempo { value: bpm }
    }
}
