//! Network byte stream serialization for the LinkAudio protocol.
//!
//! Ported from upstream `ableton/discovery/NetworkByteStreamSerializable.hpp`.
//! All values are written in network (big-endian) byte order. Strings and
//! vectors are prefixed by a `u32` element/byte count, which is why they are
//! not encoded with the crate-wide `bincode` configuration (that would use a
//! `u64` length prefix for variable length data).

use super::error::{AudioError, Result};

/// A cursor over a byte slice that decodes values in network byte order.
#[derive(Debug, Clone)]
pub struct ByteStreamReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteStreamReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(AudioError::Range("parsing type from byte stream failed"));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn read_i8(&mut self) -> Result<i8> {
        Ok(self.read_u8()? as i8)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        Ok(self.read_u64()? as i64)
    }

    pub fn read_f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// Reads a `u32`-length-prefixed UTF-8 string. Invalid UTF-8 is replaced
    /// rather than rejected so that a peer with a non-UTF-8 name does not
    /// invalidate an otherwise well-formed message.
    pub fn read_string(&mut self) -> Result<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Reads a `u32`-count-prefixed sequence, decoding each element with `f`.
    pub fn read_vec<T, F>(&mut self, mut f: F) -> Result<Vec<T>>
    where
        F: FnMut(&mut Self) -> Result<T>,
    {
        let count = self.read_u32()? as usize;
        let mut out = Vec::with_capacity(count.min(self.remaining()));
        for _ in 0..count {
            if self.is_empty() {
                break;
            }
            out.push(f(self)?);
        }
        Ok(out)
    }
}

/// Appends values to a byte buffer in network byte order.
pub trait ByteStreamWrite {
    fn write_u8(&mut self, v: u8);
    fn write_i8(&mut self, v: i8);
    fn write_u16(&mut self, v: u16);
    fn write_i16(&mut self, v: i16);
    fn write_u32(&mut self, v: u32);
    fn write_i32(&mut self, v: i32);
    fn write_u64(&mut self, v: u64);
    fn write_i64(&mut self, v: i64);
    fn write_f64(&mut self, v: f64);
    fn write_string(&mut self, v: &str);
}

impl ByteStreamWrite for Vec<u8> {
    fn write_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn write_i8(&mut self, v: i8) {
        self.push(v as u8);
    }

    fn write_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_i16(&mut self, v: i16) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_i32(&mut self, v: i32) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_i64(&mut self, v: i64) {
        self.extend_from_slice(&v.to_be_bytes());
    }

    fn write_f64(&mut self, v: f64) {
        self.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    fn write_string(&mut self, v: &str) {
        self.write_u32(v.len() as u32);
        self.extend_from_slice(v.as_bytes());
    }
}

/// Size of a `u32`-length-prefixed string in the byte stream.
pub fn size_of_string(s: &str) -> u32 {
    core::mem::size_of::<u32>() as u32 + s.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_roundtrip_uses_u32_length_prefix() {
        let mut buf = Vec::new();
        buf.write_string("hello");
        assert_eq!(buf, vec![0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o']);
        assert_eq!(size_of_string("hello"), 9);

        let mut reader = ByteStreamReader::new(&buf);
        assert_eq!(reader.read_string().unwrap(), "hello");
        assert!(reader.is_empty());
    }

    #[test]
    fn integers_roundtrip_big_endian() {
        let mut buf = Vec::new();
        buf.write_u16(0x1234);
        buf.write_i16(-2);
        buf.write_u32(0xdead_beef);
        buf.write_u64(0x0102_0304_0506_0708);
        buf.write_f64(120.5);

        assert_eq!(buf[0], 0x12);

        let mut reader = ByteStreamReader::new(&buf);
        assert_eq!(reader.read_u16().unwrap(), 0x1234);
        assert_eq!(reader.read_i16().unwrap(), -2);
        assert_eq!(reader.read_u32().unwrap(), 0xdead_beef);
        assert_eq!(reader.read_u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(reader.read_f64().unwrap(), 120.5);
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let buf = vec![0u8, 1];
        let mut reader = ByteStreamReader::new(&buf);
        assert!(reader.read_u32().is_err());
    }

    #[test]
    fn vec_roundtrip() {
        let mut buf = Vec::new();
        buf.write_u32(3);
        for v in [10u16, 20, 30] {
            buf.write_u16(v);
        }

        let mut reader = ByteStreamReader::new(&buf);
        let values = reader.read_vec(|r| r.read_u16()).unwrap();
        assert_eq!(values, vec![10, 20, 30]);
    }
}
