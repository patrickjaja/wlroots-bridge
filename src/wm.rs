//! Window queries + activation over the foreign-toplevel protocols.
//!
//! Prefers `zwlr_foreign_toplevel_management_unstable_v1`: it lists toplevels
//! with `app_id` / `title` / `state` and supports `activate`. All three target
//! compositors advertise it - Sway, Hyprland, AND Niri (niri's
//! `src/protocols/foreign_toplevel.rs` implements the wlr manager with an
//! `activate` handler alongside the ext list), so `activate-window` works on all
//! three. Falls back to `ext_foreign_toplevel_list_v1` (staging) for listing
//! only on any compositor that advertises the ext protocol but not the wlr
//! manager - that protocol has no activate and no state, so `activate-window`
//! errors and windows report neutral state. None of Sway/Hyprland/Niri actually
//! take this fallback path.
//!
//! ## Contract deviations (documented in DESIGN.md)
//!
//! Neither protocol exposes window geometry, so every `WindowInfo.geometry` is
//! `{0,0,0,0}` and `app-under-point` returns an empty result (best-effort
//! unsupported). `exclude_from_capture` is always false. `resource_class` /
//! `desktop_file_name` are both set to the `app_id`; `resource_name` is null.

use anyhow::{Result, bail};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};

use crate::conn::Conn;
use crate::output::{AppRef, Rect, WindowInfo};

/// A collected toplevel, protocol-agnostic.
#[derive(Debug, Clone, Default)]
struct Toplevel {
    /// Synthetic stable id: the protocol object id as a decimal string, or (for
    /// the ext protocol) its `identifier` string when provided.
    id: String,
    app_id: Option<String>,
    title: Option<String>,
    activated: bool,
    minimized: bool,
    /// Order the compositor announced this toplevel (used for stacking_order).
    order: usize,
}

impl Toplevel {
    fn to_window_info(&self) -> WindowInfo {
        let app_id = self.app_id.clone().filter(|s| !s.trim().is_empty());
        WindowInfo {
            id: self.id.clone(),
            title: self.title.clone().unwrap_or_default(),
            geometry: Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            pid: None,
            desktop_file_name: app_id.clone(),
            resource_class: app_id.clone(),
            resource_name: None,
            window_role: None,
            window_type: None,
            is_dock: Some(false),
            is_desktop: Some(false),
            is_visible: Some(!self.minimized),
            is_minimized: Some(self.minimized),
            is_normal_window: Some(true),
            is_dialog: Some(false),
            transient: Some(false),
            transient_for: None,
            output: None,
            stacking_order: self.order,
            is_active: self.activated,
            exclude_from_capture: false,
            keep_above: Some(false),
        }
    }

    /// Bundle id derivation: app_id (stripped of a trailing `.desktop`), else id.
    fn bundle_id(&self) -> Option<String> {
        for cand in [self.app_id.as_deref(), Some(self.id.as_str())]
            .into_iter()
            .flatten()
        {
            let v = cand.trim();
            if !v.is_empty() {
                return Some(v.trim_end_matches(".desktop").to_owned());
            }
        }
        None
    }

    fn to_app_ref(&self) -> Option<AppRef> {
        let bundle_id = self.bundle_id()?;
        let display_name = self
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| bundle_id.clone());
        Some(AppRef {
            bundle_id,
            display_name,
        })
    }
}

// --- wlr foreign-toplevel state ---

struct WlrState {
    toplevels: Vec<Toplevel>,
    /// Live handles, kept so we can call `activate` on the matching one.
    handles: Vec<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
    /// `true` once the manager sends `finished` / initial dump is done.
    done: bool,
}

impl WlrState {
    fn index_of(
        &mut self,
        handle: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    ) -> usize {
        if let Some(i) = self.handles.iter().position(|h| h.id() == handle.id()) {
            return i;
        }
        let order = self.toplevels.len();
        self.toplevels.push(Toplevel {
            id: format!("{}", handle.id().protocol_id()),
            order,
            ..Default::default()
        });
        self.handles.push(handle.clone());
        self.toplevels.len() - 1
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        _mgr: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_manager_v1::Event;
        match event {
            Event::Toplevel { toplevel } => {
                // Register the handle so subsequent per-handle events land.
                state.index_of(&toplevel);
            }
            Event::Finished => state.done = true,
            _ => {}
        }
    }

    // The manager's `toplevel` event carries a new-id (the handle), so we must
    // declare how to build its user data.
    wayland_client::event_created_child!(WlrState, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        handle: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let idx = state.index_of(handle);
        match event {
            Event::AppId { app_id } => state.toplevels[idx].app_id = Some(app_id),
            Event::Title { title } => state.toplevels[idx].title = Some(title),
            Event::State { state: st } => {
                // `st` is a byte array of u32 state enum values.
                let mut activated = false;
                let mut minimized = false;
                for chunk in st.chunks_exact(4) {
                    let v = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    // wlr state enum: 0=maximized,1=minimized,2=activated,3=fullscreen
                    if v == 1 {
                        minimized = true;
                    }
                    if v == 2 {
                        activated = true;
                    }
                }
                state.toplevels[idx].activated = activated;
                state.toplevels[idx].minimized = minimized;
            }
            Event::Closed => {
                // Mark as closed by clearing its id so it drops out of the list.
                state.toplevels[idx].id.clear();
            }
            _ => {}
        }
    }
}

// --- ext foreign-toplevel-list state (list only, Niri) ---

struct ExtState {
    toplevels: Vec<Toplevel>,
    handles: Vec<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1>,
}

impl ExtState {
    fn index_of(
        &mut self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) -> usize {
        if let Some(i) = self.handles.iter().position(|h| h.id() == handle.id()) {
            return i;
        }
        let order = self.toplevels.len();
        self.toplevels.push(Toplevel {
            id: format!("{}", handle.id().protocol_id()),
            order,
            ..Default::default()
        });
        self.handles.push(handle.clone());
        self.toplevels.len() - 1
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for ExtState {
    fn event(
        state: &mut Self,
        _list: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.index_of(&toplevel);
        }
    }

    wayland_client::event_created_child!(ExtState, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for ExtState {
    fn event(
        state: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        let idx = state.index_of(handle);
        match event {
            Event::AppId { app_id } => state.toplevels[idx].app_id = Some(app_id),
            Event::Title { title } => state.toplevels[idx].title = Some(title),
            Event::Identifier { identifier } => {
                // Prefer the stable identifier as the id when the protocol gives one.
                if !identifier.is_empty() {
                    state.toplevels[idx].id = identifier;
                }
            }
            Event::Closed => state.toplevels[idx].id.clear(),
            _ => {}
        }
    }
}

/// Collect toplevels via whichever foreign-toplevel protocol is advertised.
///
/// Returns the toplevels and, when the wlr protocol is in use, the live handles
/// (indexed parallel to the returned Vec after filtering closed ones is NOT
/// done here - callers that need activation use [`activate_window`], which does
/// its own wlr roundtrip).
fn collect_toplevels(conn: &Conn) -> Result<Vec<Toplevel>> {
    if let Some(g) = conn.globals.foreign_toplevel_wlr {
        let mut queue = conn.conn.new_event_queue::<WlrState>();
        let qh = queue.handle();
        let _mgr: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 =
            conn.registry.bind(g.name, g.version.min(3), &qh, ());
        let mut state = WlrState {
            toplevels: Vec::new(),
            handles: Vec::new(),
            done: false,
        };
        // Two roundtrips: the first delivers the toplevel handles, the second
        // their app_id/title/state events.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        let _ = state.done;
        Ok(state
            .toplevels
            .into_iter()
            .filter(|t| !t.id.is_empty())
            .collect())
    } else if let Some(g) = conn.globals.foreign_toplevel_ext {
        let mut queue = conn.conn.new_event_queue::<ExtState>();
        let qh = queue.handle();
        let _list: ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1 =
            conn.registry.bind(g.name, g.version.min(1), &qh, ());
        let mut state = ExtState {
            toplevels: Vec::new(),
            handles: Vec::new(),
        };
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(state
            .toplevels
            .into_iter()
            .filter(|t| !t.id.is_empty())
            .collect())
    } else {
        bail!("compositor advertises no foreign-toplevel protocol (wlr or ext)");
    }
}

/// List windows.
pub fn windows(conn: &Conn) -> Result<Vec<WindowInfo>> {
    let toplevels = collect_toplevels(conn)?;
    Ok(toplevels.iter().map(Toplevel::to_window_info).collect())
}

/// The frontmost (activated) app, or the first toplevel if none is activated.
pub fn frontmost_app(conn: &Conn) -> Result<Option<AppRef>> {
    let toplevels = collect_toplevels(conn)?;
    let chosen = toplevels
        .iter()
        .find(|t| t.activated)
        .or_else(|| toplevels.first());
    Ok(chosen.and_then(Toplevel::to_app_ref))
}

/// App under a point: unsupported on Wayland (no geometry in the protocols).
/// Returns null for JSON parity; documented as best-effort-unsupported.
pub fn app_under_point(_conn: &Conn, _x: i32, _y: i32) -> Result<Option<AppRef>> {
    Ok(None)
}

/// Activate (raise + focus) a window by id.
///
/// Only supported on the wlr protocol (ext has no activate). We re-run the wlr
/// collection to get live handles, find the one whose synthetic id matches, and
/// send `activate` with a seat.
pub fn activate_window(conn: &Conn, window: &str) -> Result<serde_json::Value> {
    let g = conn.globals.foreign_toplevel_wlr.ok_or_else(|| {
        anyhow::anyhow!(
            "activate-window requires zwlr_foreign_toplevel_management (not advertised)"
        )
    })?;

    let mut queue = conn.conn.new_event_queue::<WlrState>();
    let qh = queue.handle();
    let _mgr: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 =
        conn.registry.bind(g.name, g.version.min(3), &qh, ());

    // Bind a seat for the activate request.
    let seat = conn.bind_seat_for::<WlrState>(&qh)?;

    let mut state = WlrState {
        toplevels: Vec::new(),
        handles: Vec::new(),
        done: false,
    };
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let idx = state
        .toplevels
        .iter()
        .position(|t| t.id == window)
        .ok_or_else(|| anyhow::anyhow!("window `{window}` not found"))?;

    state.handles[idx].activate(&seat);
    queue.roundtrip(&mut state)?;

    Ok(serde_json::json!({ "activated": window }))
}

// A tiny extension so activate can bind a seat under WlrState's Dispatch impls.
impl Conn {
    fn bind_seat_for<D>(
        &self,
        qh: &QueueHandle<D>,
    ) -> Result<wayland_client::protocol::wl_seat::WlSeat>
    where
        D: Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> + 'static,
    {
        let g = self
            .globals
            .seat
            .ok_or_else(|| anyhow::anyhow!("compositor does not advertise wl_seat"))?;
        Ok(self.registry.bind(g.name, g.version.min(7), qh, ()))
    }
}

wayland_client::delegate_noop!(WlrState: ignore wayland_client::protocol::wl_seat::WlSeat);

#[cfg(test)]
mod tests {
    use super::*;

    fn tl(
        id: &str,
        app_id: Option<&str>,
        title: Option<&str>,
        activated: bool,
        minimized: bool,
    ) -> Toplevel {
        Toplevel {
            id: id.to_owned(),
            app_id: app_id.map(str::to_owned),
            title: title.map(str::to_owned),
            activated,
            minimized,
            order: 0,
        }
    }

    #[test]
    fn window_info_maps_app_id_to_class_and_desktop() {
        let w = tl("5", Some("firefox"), Some("Mozilla Firefox"), true, false).to_window_info();
        assert_eq!(w.id, "5");
        assert_eq!(w.title, "Mozilla Firefox");
        assert_eq!(w.desktop_file_name.as_deref(), Some("firefox"));
        assert_eq!(w.resource_class.as_deref(), Some("firefox"));
        assert_eq!(w.resource_name, None);
        assert!(w.is_active);
        assert_eq!(w.is_minimized, Some(false));
        assert_eq!(w.is_visible, Some(true));
        // Geometry is always zero on wlroots.
        assert_eq!(w.geometry.width, 0);
        assert_eq!(w.geometry.height, 0);
        assert!(!w.exclude_from_capture);
    }

    #[test]
    fn minimized_window_is_not_visible() {
        let w = tl("7", Some("code"), Some("editor"), false, true).to_window_info();
        assert_eq!(w.is_minimized, Some(true));
        assert_eq!(w.is_visible, Some(false));
    }

    #[test]
    fn bundle_id_strips_desktop_suffix() {
        let t = tl(
            "1",
            Some("org.kde.kcalc.desktop"),
            Some("KCalc"),
            false,
            false,
        );
        assert_eq!(t.bundle_id().as_deref(), Some("org.kde.kcalc"));
    }

    #[test]
    fn bundle_id_falls_back_to_id() {
        let t = tl("42", None, None, false, false);
        assert_eq!(t.bundle_id().as_deref(), Some("42"));
    }

    #[test]
    fn app_ref_uses_title_then_bundle() {
        let t = tl("1", Some("firefox"), Some("Home"), true, false);
        let r = t.to_app_ref().unwrap();
        assert_eq!(r.bundle_id, "firefox");
        assert_eq!(r.display_name, "Home");

        let t = tl("1", Some("firefox"), None, true, false);
        let r = t.to_app_ref().unwrap();
        assert_eq!(r.display_name, "firefox");
    }
}
