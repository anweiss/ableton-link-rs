use std::{mem, time::Duration};

use bincode::{Decode, Encode};

use crate::discovery::{payload::PayloadEntryHeader, ENCODING_CONFIG};

use super::{beats::Beats, ghostxform::GhostXForm, timeline::Timeline, Result};

pub const START_STOP_STATE_HEADER_KEY: u32 = u32::from_be_bytes(*b"stst");
pub const START_STOP_STATE_SIZE: u32 =
    (mem::size_of::<bool>() + mem::size_of::<Beats>() + mem::size_of::<Duration>()) as u32;
pub const START_STOP_STATE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: START_STOP_STATE_HEADER_KEY,
    size: START_STOP_STATE_SIZE,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionState {
    pub timeline: Timeline,
    pub start_stop_state: StartStopState,
    pub ghost_xform: GhostXForm,
}

#[derive(Clone, Copy, Debug)]
pub struct ApiState {
    timline: Timeline,
    start_stop_state: ApiStartStopState,
}

#[derive(Debug, PartialEq, Eq, Default, Copy, Clone)]
struct ApiStartStopState {
    is_playing: bool,
    time: Duration,
}

impl ApiStartStopState {
    fn new(is_playing: bool, time: Duration) -> Self {
        Self { is_playing, time }
    }
}

#[derive(Default, Debug, Clone, Copy, Encode, Decode)]
pub struct StartStopState {
    pub is_playing: bool,
    pub beats: Beats,
    pub timestamp: Duration,
}

impl StartStopState {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = START_STOP_STATE_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(&self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key() {
        assert_eq!(START_STOP_STATE_HEADER_KEY, 0x73747374);
        println!("size: {}", START_STOP_STATE_SIZE);
    }
}
