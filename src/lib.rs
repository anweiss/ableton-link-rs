#![cfg_attr(not(feature = "std"), no_std)]
// `unsafe_code = "deny"` is set package-wide in Cargo.toml's `[lints.rust]`, so
// that it also covers `examples/` and `tests/`, which an inner attribute here
// would not reach. See that block for the policy on opting out.

extern crate alloc;

pub mod encoding;

#[cfg(feature = "std")]
pub mod discovery;
pub mod link;
#[cfg(feature = "audio")]
pub mod link_audio;
#[cfg(feature = "std")]
pub mod platform;
