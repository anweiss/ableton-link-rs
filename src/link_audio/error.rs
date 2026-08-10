//! Error types for the LinkAudio subsystem.

use thiserror::Error;

pub type Result<T> = core::result::Result<T, AudioError>;

#[derive(Debug, Error)]
pub enum AudioError {
    /// The byte stream ended before a complete value could be parsed, or a
    /// length field described more data than the message contains.
    #[error("byte stream range error: {0}")]
    Range(&'static str),
    /// A message was structurally valid but semantically rejected.
    #[error("invalid message: {0}")]
    Invalid(&'static str),
    /// The encoded message exceeded the maximum LinkAudio message size.
    #[error("exceeded maximum message size ({0} > {1})")]
    MessageTooLarge(usize, usize),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
