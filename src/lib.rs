#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod encoding;

#[cfg(feature = "std")]
pub mod discovery;
pub mod link;
#[cfg(feature = "audio")]
pub mod link_audio;
#[cfg(feature = "std")]
pub mod platform;

// negative control for the README maintenance check; branch is deleted after.
