//! Environment / compositor diagnostics (CLI contract v1 `doctor`).
//!
//! Always succeeds (exit 0), even with no compositor: the required contract keys
//! are reported with `backend_reachable:false` and the connection error in
//! `backend_error`. When the compositor answers, the Wayland display, detected
//! wlroots compositor (from `SWAYSOCK` / `HYPRLAND_INSTANCE_SIGNATURE` /
//! `NIRI_SOCKET`) and the advertised globals we depend on are added as extras.

use std::time::Duration;

use crate::conn::{Conn, detect_compositor};

/// Budget for the IPC probe behind `window_geometry` / `screenshot_mask`.
const MASK_PROBE_BUDGET: Duration = Duration::from_millis(1500);
/// The whole doctor stays under this (the launcher kills it at 5 s).
const DOCTOR_BUDGET: Duration = Duration::from_millis(4700);
use crate::output::{DoctorGlobals, DoctorReport};

/// Build the report. Never fails: a connection error becomes `backend_error`.
pub fn doctor() -> DoctorReport {
    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty());
    let connected = match &display {
        Some(_) => Conn::connect().map_err(|e| format!("{e:#}")),
        None => Err("WAYLAND_DISPLAY is not set".to_owned()),
    };

    let mut report = DoctorReport {
        contract: 1,
        bridge: "wlroots-bridge",
        version: env!("CARGO_PKG_VERSION"),
        backend: "wlroots",
        session_match: false,
        backend_reachable: false,
        backend_error: None,
        session_supported: false,
        restore_token_present: false,
        restore_token_path: None,
        daemon_running: false,
        session_socket: None,
        wayland_display: None,
        compositor: None,
        globals: None,
        atspi: serde_json::Value::Null,
        window_geometry: false,
        screenshot_mask: false,
        screenshot_mask_gaps: Vec::new(),
    };
    // The mask probe (the full IPC read a masked capture needs) runs beside the
    // accessibility doctor so the two together stay under the launcher's 5 s.
    let started = std::time::Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::ipc::snapshot(true, MASK_PROBE_BUDGET).ok());
    });
    report.atspi = crate::ax::doctor_atspi();
    let wait = DOCTOR_BUDGET
        .saturating_sub(started.elapsed())
        .clamp(Duration::from_millis(100), MASK_PROBE_BUDGET);
    let snap = rx.recv_timeout(wait).ok().flatten();

    match connected {
        Err(error) => report.backend_error = Some(error),
        Ok(conn) => {
            let g = &conn.globals;
            report.backend_reachable = true;
            // This bridge serves the session when the compositor exposes the
            // three protocols it drives input and capture with.
            report.session_match = g.virtual_pointer.is_some()
                && g.virtual_keyboard.is_some()
                && g.screencopy.is_some();
            report.wayland_display = display;
            report.compositor = Some(detect_compositor());
            report.globals = Some(DoctorGlobals {
                virtual_pointer: g.virtual_pointer.map(|b| b.version),
                virtual_keyboard: g.virtual_keyboard.map(|b| b.version),
                screencopy: g.screencopy.map(|b| b.version),
                foreign_toplevel_wlr: g.foreign_toplevel_wlr.map(|b| b.version),
                foreign_toplevel_ext: g.foreign_toplevel_ext.map(|b| b.version),
                xdg_output: g.xdg_output_manager.map(|b| b.version),
                wl_output_count: g.outputs.len(),
                wl_seat: g.seat.map(|b| b.version),
                wl_shm: g.shm.map(|b| b.version),
            });
            if let Some(snap) = &snap {
                report.window_geometry =
                    g.foreign_toplevel_wlr.is_some() || g.foreign_toplevel_ext.is_some();
                report.screenshot_mask = report.window_geometry && g.screencopy.is_some();
                if report.screenshot_mask {
                    report.screenshot_mask_gaps = crate::ipc::mask_gaps(snap.kind).to_vec();
                }
            }
        }
    }
    report
}
