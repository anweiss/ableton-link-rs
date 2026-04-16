use core::mem;

use alloc::vec::Vec;
use crate::encoding::{self, Decode, Encode};

use super::Result;

pub const PAYLOAD_ENTRY_HEADER_SIZE: usize = mem::size_of::<u32>() + mem::size_of::<u32>();

#[derive(Debug, Clone, Copy, Default)]
pub struct PayloadEntryHeader {
    pub key: u32,
    pub size: u32,
}

impl Encode for PayloadEntryHeader {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.key.encode_to(out);
        self.size.encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl Decode for PayloadEntryHeader {
    fn decode_from(bytes: &[u8]) -> core::result::Result<(Self, usize), encoding::DecodeError> {
        let (key, n1) = u32::decode_from(bytes)?;
        let (size, n2) = u32::decode_from(&bytes[n1..])?;
        Ok((Self { key, size }, n1 + n2))
    }
}

impl PayloadEntryHeader {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(encoding::encode_to_vec(self)?)
    }
}
