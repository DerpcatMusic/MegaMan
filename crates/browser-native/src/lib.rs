//! Native desktop services. No UI host or filesystem access.
#![forbid(unsafe_code)]
pub mod dnd;
#[cfg(target_os = "linux")]
pub mod x11;
