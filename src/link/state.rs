use core::mem;

use alloc::vec::Vec;
use chrono::Duration;

use crate::ENCODING_CONFIG;

use super::{
    beats::Beats, encoding::PayloadEntryHeader, ghostxform::GhostXForm, timeline::Timeline, Result,
};

pub const START_STOP_STATE_HEADER_KEY: u32 = u32::from_be_bytes(*b"stst");
pub const START_STOP_STATE_SIZE: u32 =
    (mem::size_of::<bool>() + mem::size_of::<Beats>() + mem::size_of::<u64>()) as u32;
pub const START_STOP_STATE_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: START_STOP_STATE_HEADER_KEY,
    size: START_STOP_STATE_SIZE,
};

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
    ) -> core::result::Result<(), bincode::error::EncodeError> {
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

impl bincode::Decode<()> for StartStopState {
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
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

#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key() {
        assert_eq!(START_STOP_STATE_HEADER_KEY, 0x73747374);
        println!("size: {}", START_STOP_STATE_SIZE);
    }

    #[test]
    fn roundtrip() {
        let start_stop_state = StartStopState {
            beats: Beats { value: 123 },
            is_playing: true,
            timestamp: Duration::zero(),
        };

        let encoded = bincode::encode_to_vec(start_stop_state, ENCODING_CONFIG).unwrap();

        let (decoded, _) =
            bincode::decode_from_slice::<StartStopState, _>(&encoded[..], ENCODING_CONFIG).unwrap();

        assert_eq!(decoded, start_stop_state);
    }

    #[test]
    fn start_stop_state_default_is_not_playing() {
        let sss = StartStopState::default();
        assert!(!sss.is_playing);
        assert_eq!(sss.beats, Beats { value: 0 });
        assert_eq!(sss.timestamp, Duration::zero());
    }

    #[test]
    fn start_stop_state_roundtrip_not_playing() {
        let sss = StartStopState {
            is_playing: false,
            beats: Beats::new(16.0),
            timestamp: Duration::microseconds(5_000_000),
        };
        let encoded = bincode::encode_to_vec(sss, ENCODING_CONFIG).unwrap();
        let (decoded, _) =
            bincode::decode_from_slice::<StartStopState, _>(&encoded, ENCODING_CONFIG).unwrap();
        assert_eq!(decoded, sss);
    }

    #[test]
    fn start_stop_state_playing_transition() {
        let stopped = StartStopState::default();
        assert!(!stopped.is_playing);

        let playing = StartStopState {
            is_playing: true,
            beats: Beats::new(0.0),
            timestamp: Duration::microseconds(1_000_000),
        };
        assert!(playing.is_playing);

        // Simulate stopping again
        let stopped_again = StartStopState {
            is_playing: false,
            beats: playing.beats,
            timestamp: Duration::microseconds(2_000_000),
        };
        assert!(!stopped_again.is_playing);
        assert!(stopped_again.timestamp > playing.timestamp);
    }

    #[test]
    fn start_stop_state_with_header_roundtrip() {
        let sss = StartStopState {
            is_playing: true,
            beats: Beats::new(4.0),
            timestamp: Duration::microseconds(999_999),
        };
        let encoded = sss.encode().unwrap();
        // The encoded data includes the header
        assert!(encoded.len() > START_STOP_STATE_SIZE as usize);
    }

    #[test]
    fn client_start_stop_state_default() {
        let css = ClientStartStopState::default();
        assert!(!css.is_playing);
        assert_eq!(css.time, Duration::zero());
        assert_eq!(css.timestamp, Duration::zero());
    }

    #[test]
    fn session_state_default() {
        let ss = SessionState::default();
        assert_eq!(ss.timeline, Timeline::default());
        assert_eq!(ss.start_stop_state, StartStopState::default());
        assert_eq!(ss.ghost_x_form, GhostXForm::default());
    }

    #[test]
    fn client_state_default() {
        let cs = ClientState::default();
        assert_eq!(cs.timeline, Timeline::default());
        assert_eq!(cs.start_stop_state, ClientStartStopState::default());
    }

    #[test]
    fn controller_client_state_wraps_client_state() {
        let ccs = ControllerClientState::default();
        assert_eq!(ccs.state, ClientState::default());
    }
}
