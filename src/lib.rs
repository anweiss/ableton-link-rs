#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod encoding;

#[cfg(feature = "std")]
pub mod discovery;
pub mod link;
#[cfg(feature = "std")]
pub mod platform;
