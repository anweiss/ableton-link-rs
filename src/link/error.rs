use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("encoding error: {0}")]
    Encoding(#[from] bincode::error::EncodeError),
    #[error("decoding error: {0}")]
    Decoding(#[from] bincode::error::DecodeError),
}
