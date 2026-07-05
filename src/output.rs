//! Shared, serde-serializable result types and JSON printing helpers.
//!
//! Field names here are the contract with the JS executor
//! (`js/cu_linux_executor.js` in claude-desktop-bin). They mirror the
//! x11-bridge / kwin-portal-bridge JSON shapes 1:1 so the wlroots bridge is a
//! drop-in substitute on wlroots-Wayland sessions (Sway / Hyprland / Niri). Do
//! NOT rename fields without updating the JS parser in lockstep.
//!
//! Casing is NOT uniform and must be preserved exactly: screens/windows are
//! snake_case; screenshots/app-refs are camelCase (that is how the JS reads
//! them and how x11-bridge / kwin-portal-bridge emit them).

use anyhow::{Context, Result};
use serde::Serialize;

/// Print any serializable value as a single compact JSON object/array on stdout.
///
/// This is the ONLY success-path output the JS side reads (it does
/// `JSON.parse(stdout.trim())`), so every subcommand handler funnels its
/// result through here.
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let rendered = serde_json::to_string(value).context("failed to serialize JSON result")?;
    println!("{rendered}");
    Ok(())
}

/// A logical rectangle in Wayland global (logical) pixel coordinates.
///
/// Matches x11-bridge / kwin-portal-bridge `Rect` and the nested `geometry`
/// object the JS reads from `screens` / `windows`.
#[derive(Debug, Clone, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A single monitor/screen entry.
///
/// JS (`mapRustScreenToDisplay`) reads: `geometry.{x,y,width,height}`, `scale`,
/// `is_primary`, `name`, `id`. `id` becomes the display's `_bridgeId` and is
/// what `--display <name>` selects on.
#[derive(Debug, Clone, Serialize)]
pub struct Screen {
    pub id: String,
    pub name: String,
    pub geometry: Rect,
    /// The output's scale factor. Integer wl_output scale, or the fractional
    /// ratio derived from the xdg-output logical size vs the wl_output mode.
    pub scale: Option<f64>,
    pub refresh_millihz: Option<u32>,
    pub is_active: bool,
    pub is_primary: bool,
}

/// A screenshot of a full monitor. camelCase to match the JS
/// `screenshotResultFromRust` reader (`result.displayWidth`, `result.originX`, ...).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotResult {
    pub base64: String,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub display_id: String,
    pub origin_x: i32,
    pub origin_y: i32,
}

/// A region (zoom) capture. Matches the JS `zoom` reader (`result.base64`,
/// `result.width`, `result.height`).
#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotCapture {
    pub base64: String,
    pub width: u32,
    pub height: u32,
}

/// A window entry. Field names match x11-bridge `WindowInfo` and the JS
/// `normalizeBundleIdFromWindow` / window readers exactly. Snake_case (NOT
/// camelCase) - the JS reads `window.desktop_file_name`, `window.resource_class`,
/// `window.is_minimized`, `window.stacking_order`, etc.
///
/// wlroots-specific: the foreign-toplevel protocols do NOT expose geometry, so
/// `geometry` is always `{0,0,0,0}` and `exclude_from_capture` is always false.
/// See DESIGN.md.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub id: String,
    pub title: String,
    pub geometry: Rect,
    pub pid: Option<u32>,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub window_role: Option<String>,
    pub window_type: Option<String>,
    pub is_dock: Option<bool>,
    pub is_desktop: Option<bool>,
    pub is_visible: Option<bool>,
    pub is_minimized: Option<bool>,
    pub is_normal_window: Option<bool>,
    pub is_dialog: Option<bool>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
    /// Name of the screen this window is on; matches a `Screen::id`.
    pub output: Option<String>,
    pub stacking_order: usize,
    pub is_active: bool,
    pub exclude_from_capture: bool,
    pub keep_above: Option<bool>,
}

/// A reference up a window's transient (parent) chain. Mirrors the x11 /
/// kwin bridge `WindowAppRef`. The foreign-toplevel protocols do not model
/// parent chains, so wlroots windows never populate this.
#[derive(Debug, Clone, Serialize)]
pub struct WindowAppRef {
    pub id: String,
    pub desktop_file_name: Option<String>,
    pub resource_class: Option<String>,
    pub resource_name: Option<String>,
    pub transient: Option<bool>,
    pub transient_for: Option<Box<WindowAppRef>>,
}

/// The frontmost / under-point app identity. camelCase to match the JS
/// `commandOutputToAppRef` reader (`result.bundleId`, `result.displayName`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRef {
    pub bundle_id: String,
    pub display_name: String,
}

/// The result of a pointer action, mirroring x11 / kwin `PointerActionResult`
/// (camelCase `action`, `x`, `y`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerActionResult {
    pub action: String,
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<AppRef>,
}

/// The result of a drag, mirroring x11 / kwin `DragActionResult`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DragActionResult {
    pub action: String,
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
}

/// The result of a left-button-held state change.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ButtonStateResult {
    pub action: String,
    pub button: String,
    pub is_held: bool,
}

/// Result of a key-sequence or hold action. Mirrors `KeyboardActionResult`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardActionResult {
    pub action: String,
    pub keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Result of a `type` action. Mirrors `TypeActionResult`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeActionResult {
    pub action: String,
    pub text: String,
    pub char_count: usize,
}

/// The `doctor` report: Wayland env, detected compositor, and which globals the
/// compositor advertises with their bound versions. Human-facing only (the JS
/// never parses `doctor`).
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub wayland_display: String,
    pub compositor: String,
    pub globals: DoctorGlobals,
}

/// Presence + bound version of each global we depend on. `Some(v)` = advertised
/// and bound at version `v`; `None` = not advertised by this compositor.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorGlobals {
    pub virtual_pointer: Option<u32>,
    pub virtual_keyboard: Option<u32>,
    pub screencopy: Option<u32>,
    pub foreign_toplevel_wlr: Option<u32>,
    pub foreign_toplevel_ext: Option<u32>,
    pub xdg_output: Option<u32>,
    pub wl_output_count: usize,
    pub wl_seat: Option<u32>,
    pub wl_shm: Option<u32>,
}
