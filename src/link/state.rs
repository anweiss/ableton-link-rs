use std::mem;

use chrono::Duration;

use crate::discovery::ENCODING_CONFIG;

use super::{
    beats::Beats, ghostxform::GhostXForm, payload::PayloadEntryHeader, timeline::Timeline, Result,
};

pub const START_STOP_STATE_HEADER_KEY: u32 = u32::from_be_bytes(*b"stst");
pub const START_STOP_STATE_SIZE: u32 =
    (mem::size_of::<bool>() + mem::size_of::<Beats>() + mem::size_of::<u64>()) as u32;
pub const START_STOP_STATE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: START_STOP_STATE_HEADER_KEY,
    size: START_STOP_STATE_SIZE,
};

#[derive(Clone, Copy, Debug)]
pub struct ApiState {
    timline: Timeline,
    start_stop_state: ApiStartStopState,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
struct ApiStartStopState {
    is_playing: bool,
    time: Duration,
}

impl ApiStartStopState {
    fn new(is_playing: bool, time: Duration) -> Self {
        Self { is_playing, time }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartStopState {
    pub is_playing: bool,
    pub beats: Beats,
    pub timestamp: Duration,
}

impl Default for StartStopState {
    fn default() -> Self {
        Self {
            is_playing: false,
            beats: Beats { value: 0i64 },
            timestamp: Duration::zero(),
        }
    }
}

impl bincode::Encode for StartStopState {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> std::result::Result<(), bincode::error::EncodeError> {
        bincode::Encode::encode(
            &(
                self.is_playing,
                self.beats,
                self.timestamp.num_microseconds().unwrap(),
            ),
            encoder,
        )
    }
}

impl bincode::Decode for StartStopState {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> std::result::Result<Self, bincode::error::DecodeError> {
        let (is_playing, beats, timestamp) = bincode::Decode::decode(decoder)?;
        Ok(Self {
            is_playing,
            beats,
            timestamp: Duration::microseconds(timestamp),
        })
    }
}

impl StartStopState {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = START_STOP_STATE_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientStartStopState {
    pub is_playing: bool,
    pub time: Duration,
    pub timestamp: Duration,
}

impl Default for ClientStartStopState {
    fn default() -> Self {
        Self {
            is_playing: false,
            time: Duration::zero(),
            timestamp: Duration::zero(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControllerClientState {
    pub state: ClientState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SessionState {
    pub timeline: Timeline,
    pub start_stop_state: StartStopState,
    pub ghost_x_form: GhostXForm,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClientState {
    pub timeline: Timeline,
    pub start_stop_state: ClientStartStopState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtClientState {
    pub timeline: Timeline,
    pub start_stop_state: ClientStartStopState,
    pub timeline_timestamp: Duration,
    pub start_stop_state_timestamp: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IncomingClientState {
    pub timeline: Option<Timeline>,
    pub start_stop_state: Option<ClientStartStopState>,
    pub timeline_timestamp: Duration,
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
