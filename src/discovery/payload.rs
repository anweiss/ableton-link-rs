use std::{mem, time::Duration};

use bincode::{Decode, Encode};
use tracing::{info, warn};

use crate::link::{
    measurement::{
        MeasurementEndpointV4, MEASUREMENT_ENDPOINT_V4_HEADER_KEY, MEASUREMENT_ENDPOINT_V4_SIZE,
    },
    node::NodeState,
    sessions::{SessionMembership, SESSION_MEMBERSHIP_HEADER_KEY, SESSION_MEMBERSHIP_SIZE},
    state::{StartStopState, START_STOP_STATE_HEADER_KEY, START_STOP_STATE_SIZE},
    timeline::{Timeline, TIMELINE_HEADER_KEY, TIMELINE_SIZE},
};

use super::{Result, ENCODING_CONFIG};

pub const PAYLOAD_ENTRY_HEADER_SIZE: usize = mem::size_of::<u32>() + mem::size_of::<u32>();

pub const HOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"__ht");
pub const HOST_TIME_SIZE: u32 = mem::size_of::<Duration>() as u32;
pub const HOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: HOST_TIME_HEADER_KEY,
    size: HOST_TIME_SIZE,
};

pub const GHOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"__gt");
pub const GHOST_TIME_SIZE: u32 = mem::size_of::<Duration>() as u32;
pub const GHOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: GHOST_TIME_HEADER_KEY,
    size: GHOST_TIME_SIZE,
};

pub const PREV_GHOST_TIME_HEADER_KEY: u32 = u32::from_be_bytes(*b"_pgt");
pub const PREV_GHOST_TIME_SIZE: u32 = mem::size_of::<Duration>() as u32;
pub const PREV_GHOST_TIME_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: PREV_GHOST_TIME_HEADER_KEY,
    size: PREV_GHOST_TIME_SIZE,
};

#[derive(Default, Debug)]
pub struct Payload {
    pub entries: Vec<PayloadEntry>,
}

impl Payload {
    pub fn size(&self) -> u32 {
        let mut size = 0;
        for entry in &self.entries {
            size += PAYLOAD_ENTRY_HEADER_SIZE as u32 + entry.size();
        }

        size
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = Vec::new();

        for entry in &self.entries {
            let mut encoded_entry = entry.encode()?;
            encoded.append(&mut encoded_entry);
        }

        Ok(encoded)
    }
}

impl From<NodeState> for Payload {
    fn from(value: NodeState) -> Self {
        Payload {
            entries: vec![
                PayloadEntry::Timeline(value.timeline),
                PayloadEntry::SessionMembership(value.session_id.into()),
                PayloadEntry::StartStopState(value.start_stop_state),
            ],
        }
    }
}

pub fn decode(payload: &mut Payload, data: &[u8]) -> Result<()> {
    if PAYLOAD_ENTRY_HEADER_SIZE > data.len() {
        return Ok(());
    }

    let (payload_entry_header, _) = bincode::decode_from_slice::<PayloadEntryHeader, _>(
        &data[..PAYLOAD_ENTRY_HEADER_SIZE],
        ENCODING_CONFIG,
    )
    .unwrap();

    match payload_entry_header.key {
        HOST_TIME_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + HOST_TIME_SIZE as usize;
            let (entry, _) = bincode::decode_from_slice::<HostTime, _>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
                ENCODING_CONFIG,
            )?;

            // info!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::HostTime(entry));
            decode(payload, &data[decode_len..])?;
        }
        TIMELINE_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + TIMELINE_SIZE as usize;
            let (entry, _) = bincode::decode_from_slice::<Timeline, _>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
                ENCODING_CONFIG,
            )
            .unwrap();

            info!("bpm: {}", entry.tempo);

            payload.entries.push(PayloadEntry::Timeline(entry));
            decode(payload, &data[decode_len..])?;
        }
        SESSION_MEMBERSHIP_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + SESSION_MEMBERSHIP_SIZE as usize;
            let (entry, _) = bincode::decode_from_slice::<SessionMembership, _>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
                ENCODING_CONFIG,
            )?;

            // info!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::SessionMembership(entry));
            decode(payload, &data[decode_len..])?;
        }
        START_STOP_STATE_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + START_STOP_STATE_SIZE as usize;
            let (entry, _) = bincode::decode_from_slice::<StartStopState, _>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
                ENCODING_CONFIG,
            )?;

            // info!("decoded payload entry {:?}", entry);

            payload.entries.push(PayloadEntry::StartStopState(entry));
            decode(payload, &data[decode_len..])?;
        }
        MEASUREMENT_ENDPOINT_V4_HEADER_KEY => {
            let decode_len = PAYLOAD_ENTRY_HEADER_SIZE + MEASUREMENT_ENDPOINT_V4_SIZE as usize;
            let (entry, _) = bincode::decode_from_slice::<MeasurementEndpointV4, _>(
                &data[PAYLOAD_ENTRY_HEADER_SIZE..decode_len],
                ENCODING_CONFIG,
            )?;

            // info!("decoded payload entry {:?}", entry);

            payload
                .entries
                .push(PayloadEntry::MeasurementEndpointV4(entry));
            decode(payload, &data[decode_len..])?;
        }
        _ => {
            warn!("unknown payload entry key {:x}", payload_entry_header.key);
            todo!()
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, Encode, Decode, Default)]
pub struct PayloadEntryHeader {
    pub key: u32,
    // payload entry size
    pub size: u32,
}

impl PayloadEntryHeader {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(bincode::encode_to_vec(self, ENCODING_CONFIG)?)
    }
}

#[derive(Debug)]
pub enum PayloadEntry {
    HostTime(HostTime),
    GhostTime(GhostTime),
    PrevGhostTime(PrevGhostTime),
    Timeline(Timeline),
    SessionMembership(SessionMembership),
    StartStopState(StartStopState),
    MeasurementEndpointV4(MeasurementEndpointV4),
}

impl PayloadEntry {
    pub fn size(&self) -> u32 {
        match self {
            PayloadEntry::HostTime(_) => HOST_TIME_SIZE,
            PayloadEntry::GhostTime(_) => GHOST_TIME_SIZE,
            PayloadEntry::PrevGhostTime(_) => PREV_GHOST_TIME_SIZE,
            PayloadEntry::Timeline(_) => TIMELINE_SIZE,
            PayloadEntry::SessionMembership(_) => SESSION_MEMBERSHIP_SIZE,
            PayloadEntry::StartStopState(_) => START_STOP_STATE_SIZE,
            PayloadEntry::MeasurementEndpointV4(_) => MEASUREMENT_ENDPOINT_V4_SIZE,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            PayloadEntry::HostTime(host_time) => host_time.encode(),
            PayloadEntry::GhostTime(ghost_time) => ghost_time.encode(),
            PayloadEntry::PrevGhostTime(prev_ghost_time) => prev_ghost_time.encode(),
            PayloadEntry::Timeline(timeline) => timeline.encode(),
            PayloadEntry::SessionMembership(session_membership) => session_membership.encode(),
            PayloadEntry::StartStopState(start_stop_state) => start_stop_state.encode(),
            PayloadEntry::MeasurementEndpointV4(measurement_endpoint_v4) => {
                measurement_endpoint_v4.encode()
            }
        }
    }
}

#[derive(Debug, Clone, Decode, Default)]
pub struct HostTime {
    pub time: Duration,
}

impl HostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = HOST_TIME_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(
            self.time.as_micros() as u64,
            ENCODING_CONFIG,
        )?);
        Ok(encoded)
    }
}

#[derive(Debug, Clone, Decode, Default)]
pub struct GhostTime {
    pub time: Duration,
}

impl GhostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = GHOST_TIME_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(
            self.time.as_micros() as u64,
            ENCODING_CONFIG,
        )?);
        Ok(encoded)
    }
}

#[derive(Debug, Clone, Decode, Default)]
pub struct PrevGhostTime {
    pub time: Duration,
}

impl PrevGhostTime {
    pub fn new(time: Duration) -> Self {
        Self { time }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = PREV_GHOST_TIME_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(
            self.time.as_micros() as u64,
            ENCODING_CONFIG,
        )?);
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_time_header() {
        // assert key is 0x5f5f6874
        assert_eq!(HOST_TIME_HEADER_KEY, 0x5f5f6874, "unexpected byte order");
        // assert size is 16
        assert_eq!(HOST_TIME_SIZE, 16, "unexpected size");
    }

    #[test]
    fn ghost_time_header() {
        // assert key is 0x5f5f6874
        assert_eq!(GHOST_TIME_HEADER_KEY, 0x5f5f6774, "unexpected byte order");
        // assert size is 16
        assert_eq!(GHOST_TIME_SIZE, 16, "unexpected size");
    }

    #[test]
    fn prev_ghost_time_header() {
        // assert key is 0x5f5f6874
        assert_eq!(
            PREV_GHOST_TIME_HEADER_KEY, 0x5f706774,
            "unexpected byte order"
        );

        // assert size is 16
        assert_eq!(PREV_GHOST_TIME_SIZE, 16, "unexpected size");
    }
}
