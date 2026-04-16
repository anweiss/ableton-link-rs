//! Minimal big-endian fixed-int encoding for the Ableton Link wire protocol.
//!
//! This replaces the sunset `bincode` crate with a tiny, `no_std`-compatible
//! encoding layer that produces byte-identical output to the previous
//! `bincode::config::standard().with_big_endian().with_fixed_int_encoding()`.

extern crate alloc;
use alloc::vec::Vec;
use core::fmt;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum EncodeError {
    // Encoding to a Vec<u8> is infallible in practice, but we keep this type
    // so call-sites can use `?` uniformly.
}

impl fmt::Display for EncodeError {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

#[derive(Debug)]
pub enum DecodeError {
    UnexpectedEnd,
    InvalidBool(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEnd => write!(f, "unexpected end of input"),
            DecodeError::InvalidBool(v) => write!(f, "invalid bool value: {}", v),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

#[cfg(feature = "std")]
impl std::error::Error for EncodeError {}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

pub trait Encode {
    fn encode_to(&self, out: &mut Vec<u8>);
    fn encoded_size(&self) -> usize;
}

pub trait Decode: Sized {
    /// Decode from `bytes`, returning `(value, bytes_consumed)`.
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError>;
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

pub fn encode_to_vec<T: Encode>(v: &T) -> Result<Vec<u8>, EncodeError> {
    let mut buf = Vec::with_capacity(v.encoded_size());
    v.encode_to(&mut buf);
    Ok(buf)
}

pub fn decode_from_slice<T: Decode>(bytes: &[u8]) -> Result<(T, usize), DecodeError> {
    T::decode_from(bytes)
}

// ---------------------------------------------------------------------------
// Primitive impls
// ---------------------------------------------------------------------------

impl Encode for u8 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(*self);
    }
    fn encoded_size(&self) -> usize {
        1
    }
}

impl Decode for u8 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::UnexpectedEnd);
        }
        Ok((bytes[0], 1))
    }
}

impl Encode for u16 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn encoded_size(&self) -> usize {
        2
    }
}

impl Decode for u16 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 2 {
            return Err(DecodeError::UnexpectedEnd);
        }
        Ok((u16::from_be_bytes([bytes[0], bytes[1]]), 2))
    }
}

impl Encode for u32 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn encoded_size(&self) -> usize {
        4
    }
}

impl Decode for u32 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 4 {
            return Err(DecodeError::UnexpectedEnd);
        }
        Ok((
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            4,
        ))
    }
}

impl Encode for u64 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl Decode for u64 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 8 {
            return Err(DecodeError::UnexpectedEnd);
        }
        let arr: [u8; 8] = bytes[..8].try_into().unwrap();
        Ok((u64::from_be_bytes(arr), 8))
    }
}

impl Encode for i64 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl Decode for i64 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 8 {
            return Err(DecodeError::UnexpectedEnd);
        }
        let arr: [u8; 8] = bytes[..8].try_into().unwrap();
        Ok((i64::from_be_bytes(arr), 8))
    }
}

impl Encode for f64 {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn encoded_size(&self) -> usize {
        8
    }
}

impl Decode for f64 {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 8 {
            return Err(DecodeError::UnexpectedEnd);
        }
        let arr: [u8; 8] = bytes[..8].try_into().unwrap();
        Ok((f64::from_be_bytes(arr), 8))
    }
}

impl Encode for bool {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(if *self { 1 } else { 0 });
    }
    fn encoded_size(&self) -> usize {
        1
    }
}

impl Decode for bool {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.is_empty() {
            return Err(DecodeError::UnexpectedEnd);
        }
        match bytes[0] {
            0 => Ok((false, 1)),
            1 => Ok((true, 1)),
            v => Err(DecodeError::InvalidBool(v)),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed-size byte arrays
// ---------------------------------------------------------------------------

impl<const N: usize> Encode for [u8; N] {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
    fn encoded_size(&self) -> usize {
        N
    }
}

impl<const N: usize> Decode for [u8; N] {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < N {
            return Err(DecodeError::UnexpectedEnd);
        }
        let mut arr = [0u8; N];
        arr.copy_from_slice(&bytes[..N]);
        Ok((arr, N))
    }
}

// ---------------------------------------------------------------------------
// Tuples
// ---------------------------------------------------------------------------

impl<A: Encode, B: Encode> Encode for (A, B) {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.0.encode_to(out);
        self.1.encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        self.0.encoded_size() + self.1.encoded_size()
    }
}

impl<A: Decode, B: Decode> Decode for (A, B) {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        let (a, n1) = A::decode_from(bytes)?;
        let (b, n2) = B::decode_from(&bytes[n1..])?;
        Ok(((a, b), n1 + n2))
    }
}

impl<A: Encode, B: Encode, C: Encode> Encode for (A, B, C) {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.0.encode_to(out);
        self.1.encode_to(out);
        self.2.encode_to(out);
    }
    fn encoded_size(&self) -> usize {
        self.0.encoded_size() + self.1.encoded_size() + self.2.encoded_size()
    }
}

impl<A: Decode, B: Decode, C: Decode> Decode for (A, B, C) {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        let (a, n1) = A::decode_from(bytes)?;
        let (b, n2) = B::decode_from(&bytes[n1..])?;
        let (c, n3) = C::decode_from(&bytes[n1 + n2..])?;
        Ok(((a, b, c), n1 + n2 + n3))
    }
}

// Ipv4Addr: encoded as 4 big-endian bytes (same as u32::from / Ipv4Addr::from)
#[cfg(feature = "std")]
impl Encode for std::net::Ipv4Addr {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.octets());
    }
    fn encoded_size(&self) -> usize {
        4
    }
}

#[cfg(feature = "std")]
impl Decode for std::net::Ipv4Addr {
    fn decode_from(bytes: &[u8]) -> Result<(Self, usize), DecodeError> {
        if bytes.len() < 4 {
            return Err(DecodeError::UnexpectedEnd);
        }
        Ok((
            std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]),
            4,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_roundtrip() {
        let v: u8 = 0xAB;
        let enc = encode_to_vec(&v).unwrap();
        assert_eq!(enc, [0xAB]);
        let (dec, n) = decode_from_slice::<u8>(&enc).unwrap();
        assert_eq!(dec, v);
        assert_eq!(n, 1);
    }

    #[test]
    fn u16_big_endian() {
        let v: u16 = 0x1234;
        let enc = encode_to_vec(&v).unwrap();
        assert_eq!(enc, [0x12, 0x34]);
        let (dec, n) = decode_from_slice::<u16>(&enc).unwrap();
        assert_eq!(dec, v);
        assert_eq!(n, 2);
    }

    #[test]
    fn u32_big_endian() {
        let v: u32 = 0xDEADBEEF;
        let enc = encode_to_vec(&v).unwrap();
        assert_eq!(enc, [0xDE, 0xAD, 0xBE, 0xEF]);
        let (dec, _) = decode_from_slice::<u32>(&enc).unwrap();
        assert_eq!(dec, v);
    }

    #[test]
    fn i64_roundtrip() {
        let v: i64 = -123456789;
        let enc = encode_to_vec(&v).unwrap();
        let (dec, _) = decode_from_slice::<i64>(&enc).unwrap();
        assert_eq!(dec, v);
    }

    #[test]
    fn bool_roundtrip() {
        let enc_true = encode_to_vec(&true).unwrap();
        let enc_false = encode_to_vec(&false).unwrap();
        assert_eq!(enc_true, [1]);
        assert_eq!(enc_false, [0]);
        assert!(decode_from_slice::<bool>(&enc_true).unwrap().0);
        assert!(!decode_from_slice::<bool>(&enc_false).unwrap().0);
    }

    #[test]
    fn bool_invalid() {
        assert!(matches!(
            decode_from_slice::<bool>(&[2]),
            Err(DecodeError::InvalidBool(2))
        ));
    }

    #[test]
    fn byte_array_roundtrip() {
        let v: [u8; 4] = [1, 2, 3, 4];
        let enc = encode_to_vec(&v).unwrap();
        assert_eq!(enc, [1, 2, 3, 4]);
        let (dec, n) = decode_from_slice::<[u8; 4]>(&enc).unwrap();
        assert_eq!(dec, v);
        assert_eq!(n, 4);
    }

    #[test]
    fn tuple2_roundtrip() {
        let v: (u32, u16) = (0x01020304, 0x0506);
        let enc = encode_to_vec(&v).unwrap();
        assert_eq!(enc, [0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        let (dec, _) = decode_from_slice::<(u32, u16)>(&enc).unwrap();
        assert_eq!(dec, v);
    }

    #[test]
    fn tuple3_roundtrip() {
        let v: (u8, u16, u32) = (0xFF, 0x1234, 0xAABBCCDD);
        let enc = encode_to_vec(&v).unwrap();
        let (dec, _) = decode_from_slice::<(u8, u16, u32)>(&enc).unwrap();
        assert_eq!(dec, v);
    }

    #[test]
    fn unexpected_end() {
        assert!(matches!(
            decode_from_slice::<u32>(&[0, 1]),
            Err(DecodeError::UnexpectedEnd)
        ));
    }
}
