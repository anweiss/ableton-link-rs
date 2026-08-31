#[cfg(feature = "std")]
use thiserror::Error;

use crate::encoding;

#[derive(Debug)]
#[cfg_attr(feature = "std", derive(Error))]
pub enum Error {
    #[cfg_attr(feature = "std", error("encoding error: {0}"))]
    Encoding(#[cfg_attr(feature = "std", from)] encoding::EncodeError),
    #[cfg_attr(feature = "std", error("decoding error: {0}"))]
    Decoding(#[cfg_attr(feature = "std", from)] encoding::DecodeError),
    /// A message body exceeds `MAX_MESSAGE_SIZE`. Matches upstream's
    /// `sendUdpMessage`, which now catches an encoding failure this large
    /// and logs it rather than crashing the caller.
    #[cfg_attr(feature = "std", error("message size {0} exceeds maximum {1}"))]
    MessageTooLarge(usize, usize),
}

#[cfg(not(feature = "std"))]
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Encoding(e) => write!(f, "encoding error: {}", e),
            Error::Decoding(e) => write!(f, "decoding error: {}", e),
            Error::MessageTooLarge(size, max) => {
                write!(f, "message size {} exceeds maximum {}", size, max)
            }
        }
    }
}

#[cfg(not(feature = "std"))]
impl From<encoding::EncodeError> for Error {
    fn from(e: encoding::EncodeError) -> Self {
        Error::Encoding(e)
    }
}

#[cfg(not(feature = "std"))]
impl From<encoding::DecodeError> for Error {
    fn from(e: encoding::DecodeError) -> Self {
        Error::Decoding(e)
    }
}
