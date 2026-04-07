#[cfg(feature = "std")]
use thiserror::Error;

#[derive(Debug)]
#[cfg_attr(feature = "std", derive(Error))]
pub enum Error {
    #[cfg_attr(feature = "std", error("encoding error: {0}"))]
    Encoding(#[cfg_attr(feature = "std", from)] bincode::error::EncodeError),
    #[cfg_attr(feature = "std", error("decoding error: {0}"))]
    Decoding(#[cfg_attr(feature = "std", from)] bincode::error::DecodeError),
}

#[cfg(not(feature = "std"))]
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Encoding(e) => write!(f, "encoding error: {}", e),
            Error::Decoding(e) => write!(f, "decoding error: {}", e),
        }
    }
}

#[cfg(not(feature = "std"))]
impl From<bincode::error::EncodeError> for Error {
    fn from(e: bincode::error::EncodeError) -> Self {
        Error::Encoding(e)
    }
}

#[cfg(not(feature = "std"))]
impl From<bincode::error::DecodeError> for Error {
    fn from(e: bincode::error::DecodeError) -> Self {
        Error::Decoding(e)
    }
}
