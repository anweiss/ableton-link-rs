use core::mem;

use alloc::vec::Vec;
use bincode::{Decode, Encode};

use crate::ENCODING_CONFIG;

use super::Result;

pub const PAYLOAD_ENTRY_HEADER_SIZE: usize = mem::size_of::<u32>() + mem::size_of::<u32>();

#[derive(Debug, Clone, Copy, Encode, Decode, Default)]
pub struct PayloadEntryHeader {
    pub key: u32,
    pub size: u32,
}

impl PayloadEntryHeader {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(bincode::encode_to_vec(self, ENCODING_CONFIG)?)
    }
}
