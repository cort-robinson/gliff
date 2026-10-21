//! The Windows client: a Win32 window that shows one remote machine,
//! decoding with Media Foundation on a Direct3D 11 device and drawing
//! through a DXGI swap chain, or on the CPU with GDI where that is missing.
//!
//! The pure parts (key codes, keyboard layouts, the fit, clipboard image
//! conversion) build on every platform so Linux CI tests them; the Win32
//! parts build only on Windows.

#![cfg_attr(not(windows), allow(dead_code))]

pub mod convert;
pub mod fit;
pub mod keys;
pub mod layout;
pub mod sps;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod gdi;
#[cfg(windows)]
pub mod gpu;
#[cfg(windows)]
mod hook;
#[cfg(windows)]
mod present;

#[cfg(windows)]
pub use app::main;
