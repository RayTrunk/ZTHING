//! Window, settings and texts of the ZTHING GUI, a client of the ZTHING service.

/// Rendered from `assets/` by build.rs, the sizes must match the ones there.
pub const WINDOW_ICON_SIZE: u32 = 256;
pub const WINDOW_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window_icon.rgba"));
pub const TRAY_ICON_SIZE: u32 = 32;
pub const TRAY_ICON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tray_icon.rgba"));

pub mod app;
pub mod config;
pub mod http;
pub mod preview;
pub mod settings;
mod shell;
pub mod text;
pub mod theme;
