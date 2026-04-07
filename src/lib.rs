#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use bincode::config::{BigEndian, Configuration, Fixint};

pub const ENCODING_CONFIG: Configuration<BigEndian, Fixint> = bincode::config::standard()
    .with_big_endian()
    .with_fixed_int_encoding();

#[cfg(feature = "std")]
pub mod discovery;
pub mod link;
#[cfg(feature = "std")]
pub mod platform;
