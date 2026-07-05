//! Output (screen) enumeration via `wl_output` + `zxdg_output_manager_v1`.
//!
//! For each advertised `wl_output` we bind the output and request its
//! `zxdg_output_v1` from the manager, then roundtrip to collect: the connector
//! name, integer scale, the current mode (physical size + refresh), and the
//! xdg-output logical position/size (in global compositor space).
//!
//! Wayland has no "primary" output. We pick the output at logical (0,0) as
//! primary/active; if none sits there, the first enumerated output. This is
//! documented in DESIGN.md and mirrors how the JS `isPrimary` display picker
//! resolves a truthy primary.

use anyhow::{Context, Result};
use wayland_client::protocol::wl_output;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::conn::{Conn, OutputEntry};
use crate::output::{Rect, Screen};

/// Per-output accumulator during the enumeration roundtrip.
struct ScreenState {
    entries: Vec<OutputEntry>,
}

impl ScreenState {
    /// Find the entry whose bound `wl_output` matches `proxy`.
    fn entry_for_output(&mut self, proxy: &wl_output::WlOutput) -> Option<&mut OutputEntry> {
        self.entries
            .iter_mut()
            .find(|e| e.wl_output.id() == proxy.id())
    }

    /// Find the entry associated with a given xdg-output proxy id (stored as the
    /// proxy's user data = the wl_output id via the index we assign).
    fn entry_by_index(&mut self, index: usize) -> Option<&mut OutputEntry> {
        self.entries.get_mut(index)
    }
}

impl Dispatch<wl_output::WlOutput, ()> for ScreenState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.entry_for_output(proxy) else {
            return;
        };
        match event {
            wl_output::Event::Scale { factor } => entry.scale = factor,
            wl_output::Event::Name { name } => entry.name = Some(name),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                // Only the "current" mode is interesting.
                let is_current = flags
                    .into_result()
                    .map(|f| f.contains(wl_output::Mode::Current))
                    .unwrap_or(false);
                if is_current {
                    entry.mode_width = Some(width);
                    entry.mode_height = Some(height);
                    entry.refresh_millihz = Some(refresh.max(0) as u32);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for ScreenState {
    fn event(
        state: &mut Self,
        _proxy: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.entry_by_index(*index) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                entry.logical_x = Some(x);
                entry.logical_y = Some(y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.logical_width = Some(width);
                entry.logical_height = Some(height);
            }
            // xdg-output name is a fallback if wl_output.name is absent.
            zxdg_output_v1::Event::Name { name } if entry.name.is_none() => {
                entry.name = Some(name);
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(ScreenState: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

/// Enumerate outputs and collect their geometry/scale.
pub fn enumerate(conn: &Conn) -> Result<Vec<OutputEntry>> {
    let mut queue = conn.conn.new_event_queue::<ScreenState>();
    let qh = queue.handle();

    // Bind every advertised wl_output.
    let mut entries = Vec::new();
    for (index, global) in conn.output_names().iter().enumerate() {
        let wl_output = conn.bind_wl_output(*global, &qh);
        entries.push(OutputEntry {
            wl_output,
            registry_name: global.name,
            name: None,
            scale: 1,
            logical_x: None,
            logical_y: None,
            logical_width: None,
            logical_height: None,
            mode_width: None,
            mode_height: None,
            refresh_millihz: None,
        });
        let _ = index;
    }

    let mut state = ScreenState { entries };

    // Request an xdg-output for each wl_output, keyed by its index so events
    // route back to the right entry.
    if let Ok(xdg_mgr) = conn.bind_xdg_output_manager(&qh) {
        for index in 0..state.entries.len() {
            let wl_output = state.entries[index].wl_output.clone();
            xdg_mgr.get_xdg_output(&wl_output, &qh, index);
        }
    }

    // Two roundtrips: the first delivers wl_output geometry events, the second
    // the xdg-output logical geometry (some compositors defer it a frame).
    queue
        .roundtrip(&mut state)
        .context("failed the output-enumeration roundtrip")?;
    queue
        .roundtrip(&mut state)
        .context("failed the xdg-output roundtrip")?;

    Ok(state.entries)
}

/// The pure geometry/scale fields of an output, decoupled from the wl_output
/// proxy so the mapping can be unit-tested without a live connection.
#[derive(Debug, Clone)]
pub struct OutputGeometry {
    pub registry_name: u32,
    pub name: Option<String>,
    pub scale: i32,
    pub logical_x: Option<i32>,
    pub logical_y: Option<i32>,
    pub logical_width: Option<i32>,
    pub logical_height: Option<i32>,
    pub mode_width: Option<i32>,
    pub mode_height: Option<i32>,
    pub refresh_millihz: Option<u32>,
}

impl OutputGeometry {
    pub fn from_entry(entry: &OutputEntry) -> Self {
        Self {
            registry_name: entry.registry_name,
            name: entry.name.clone(),
            scale: entry.scale,
            logical_x: entry.logical_x,
            logical_y: entry.logical_y,
            logical_width: entry.logical_width,
            logical_height: entry.logical_height,
            mode_width: entry.mode_width,
            mode_height: entry.mode_height,
            refresh_millihz: entry.refresh_millihz,
        }
    }
}

/// Map an output's geometry to the contract `Screen` shape.
///
/// Geometry: xdg-output logical position/size when present; else fall back to
/// (0,0) + the wl_output mode divided by the integer scale. `scale`: the
/// fractional scale derived from mode-size / logical-size when both are known
/// (captures 1.25/1.5 etc.), else the integer wl_output scale.
pub fn to_screen(geom: &OutputGeometry, is_primary: bool) -> Screen {
    let x = geom.logical_x.unwrap_or(0);
    let y = geom.logical_y.unwrap_or(0);

    let (width, height) = match (geom.logical_width, geom.logical_height) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            // Derive a logical size from the physical mode and integer scale.
            let s = geom.scale.max(1);
            (
                geom.mode_width.unwrap_or(0) / s,
                geom.mode_height.unwrap_or(0) / s,
            )
        }
    };

    // Prefer a fractional scale computed from physical vs logical size (this is
    // the effective scale a fractional-scaling compositor applies); fall back to
    // the integer wl_output scale.
    let scale = match (geom.mode_width, geom.logical_width) {
        (Some(mode_w), Some(logical_w)) if logical_w > 0 => {
            Some((mode_w as f64) / (logical_w as f64))
        }
        _ => Some(geom.scale.max(1) as f64),
    };

    let name = geom
        .name
        .clone()
        .unwrap_or_else(|| format!("output-{}", geom.registry_name));

    Screen {
        id: name.clone(),
        name,
        geometry: Rect {
            x,
            y,
            width,
            height,
        },
        scale,
        refresh_millihz: geom.refresh_millihz,
        is_active: is_primary,
        is_primary,
    }
}

/// Build the full `screens` list, choosing a primary output.
pub fn screens(conn: &Conn) -> Result<Vec<Screen>> {
    let entries = enumerate(conn)?;
    if entries.is_empty() {
        anyhow::bail!("no wl_output advertised by the compositor");
    }

    // Primary = the output at logical (0,0); else the first enumerated one.
    let primary_index = entries
        .iter()
        .position(|e| e.logical_x == Some(0) && e.logical_y == Some(0))
        .unwrap_or(0);

    Ok(entries
        .iter()
        .enumerate()
        .map(|(i, e)| to_screen(&OutputGeometry::from_entry(e), i == primary_index))
        .collect())
}

/// Resolve a `--display <name>` selector to an output entry.
///
/// With a selector: match by name, else error. Without: the output at logical
/// (0,0), else the first.
pub fn resolve_output(entries: &[OutputEntry], selector: Option<&str>) -> Result<OutputEntry> {
    if entries.is_empty() {
        anyhow::bail!("no wl_output advertised by the compositor");
    }
    if let Some(selector) = selector {
        return entries
            .iter()
            .find(|e| e.name.as_deref() == Some(selector))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("display `{selector}` not found"));
    }
    Ok(entries
        .iter()
        .find(|e| e.logical_x == Some(0) && e.logical_y == Some(0))
        .or_else(|| entries.first())
        .cloned()
        .expect("non-empty checked above"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(name: &str, x: i32, y: i32, lw: i32, lh: i32, mw: i32, scale: i32) -> OutputGeometry {
        OutputGeometry {
            registry_name: 1,
            name: Some(name.to_owned()),
            scale,
            logical_x: Some(x),
            logical_y: Some(y),
            logical_width: Some(lw),
            logical_height: Some(lh),
            mode_width: Some(mw),
            mode_height: Some((mw as f64 * lh as f64 / lw as f64) as i32),
            refresh_millihz: Some(60_000),
        }
    }

    #[test]
    fn logical_geometry_used_when_present() {
        let s = to_screen(&geom("DP-1", 1920, 0, 1920, 1080, 1920, 1), false);
        assert_eq!(s.geometry.x, 1920);
        assert_eq!(s.geometry.y, 0);
        assert_eq!(s.geometry.width, 1920);
        assert_eq!(s.geometry.height, 1080);
        assert_eq!(s.name, "DP-1");
    }

    #[test]
    fn fractional_scale_derived_from_mode_vs_logical() {
        // Physical 3840 wide, logical 2560 wide -> scale 1.5.
        let s = to_screen(&geom("eDP-1", 0, 0, 2560, 1440, 3840, 2), true);
        assert!((s.scale.unwrap() - 1.5).abs() < 1e-9, "scale {:?}", s.scale);
        assert!(s.is_primary);
        assert!(s.is_active);
    }

    #[test]
    fn integer_scale_when_no_logical_size() {
        let mut g = geom("HDMI-1", 0, 0, 1920, 1080, 1920, 2);
        g.logical_width = None;
        g.logical_height = None;
        let s = to_screen(&g, false);
        // logical size derived from mode / scale.
        assert_eq!(s.geometry.width, 960);
        // scale falls back to the integer wl_output scale.
        assert!((s.scale.unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn name_falls_back_to_registry_id() {
        let mut g = geom("x", 0, 0, 800, 600, 800, 1);
        g.name = None;
        g.registry_name = 42;
        let s = to_screen(&g, false);
        assert_eq!(s.name, "output-42");
        assert_eq!(s.id, "output-42");
    }
}
