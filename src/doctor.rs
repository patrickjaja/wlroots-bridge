//! Environment / compositor diagnostics.
//!
//! Reports `WAYLAND_DISPLAY`, the detected wlroots compositor (from
//! `SWAYSOCK` / `HYPRLAND_INSTANCE_SIGNATURE` / `NIRI_SOCKET`), and which of the
//! globals we depend on the compositor advertises, with their bound versions.
//! Human-facing only (the JS never parses `doctor`); the single most useful
//! command when triaging why capture or input is failing on a given compositor.

use crate::conn::{Conn, detect_compositor};
use crate::output::{DoctorGlobals, DoctorReport};

/// Gather the Wayland environment + advertised globals into a `DoctorReport`.
pub fn doctor(conn: &Conn) -> DoctorReport {
    let wayland_display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());

    let g = &conn.globals;
    let globals = DoctorGlobals {
        virtual_pointer: g.virtual_pointer.map(|b| b.version),
        virtual_keyboard: g.virtual_keyboard.map(|b| b.version),
        screencopy: g.screencopy.map(|b| b.version),
        foreign_toplevel_wlr: g.foreign_toplevel_wlr.map(|b| b.version),
        foreign_toplevel_ext: g.foreign_toplevel_ext.map(|b| b.version),
        xdg_output: g.xdg_output_manager.map(|b| b.version),
        wl_output_count: g.outputs.len(),
        wl_seat: g.seat.map(|b| b.version),
        wl_shm: g.shm.map(|b| b.version),
    };

    DoctorReport {
        wayland_display,
        compositor: detect_compositor(),
        globals,
    }
}
