#![cfg_attr(not(feature = "std"), no_std)]
// This is a port of a C++ library, so the tempting shape for every platform
// item is a hand-written FFI shim. Prefer a vetted crate that already wraps
// the OS mechanism; reach for `unsafe` only when no such crate exists.
// Opting out is deliberate and reviewable: add `#[allow(unsafe_code)]` at the
// narrowest possible scope with a comment saying why nothing safe would do.
#![deny(unsafe_code)]

extern crate alloc;

pub mod encoding;

#[cfg(feature = "std")]
pub mod discovery;
pub mod link;
#[cfg(feature = "audio")]
pub mod link_audio;
#[cfg(feature = "std")]
pub mod platform;
