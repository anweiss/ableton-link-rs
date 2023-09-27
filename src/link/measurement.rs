use std::{
    mem,
    net::{Ipv4Addr, SocketAddrV4},
};

use bincode::{Decode, Encode};

use crate::{
    discovery::{payload::PayloadEntryHeader, ENCODING_CONFIG},
    link::Result,
};

#[derive(Default, Debug)]
pub struct MeasurementService {}

pub const MEASUREMENT_ENDPOINT_V4_HEADER_KEY: u32 = u32::from_be_bytes(*b"mep4");
pub const MEASUREMENT_ENDPOINT_V4_SIZE: u32 =
    (mem::size_of::<Ipv4Addr>() + mem::size_of::<u16>()) as u32;
pub const MEASUREMENT_ENDPOINT_V4_HEADER: PayloadEntryHeader = PayloadEntryHeader {
    key: MEASUREMENT_ENDPOINT_V4_HEADER_KEY,
    size: MEASUREMENT_ENDPOINT_V4_SIZE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub struct MeasurementEndpointV4 {
    pub endpoint: SocketAddrV4,
}

impl MeasurementEndpointV4 {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut encoded = MEASUREMENT_ENDPOINT_V4_HEADER.encode()?;
        encoded.append(&mut bincode::encode_to_vec(self.endpoint, ENCODING_CONFIG)?);
        Ok(encoded)
    }
}
