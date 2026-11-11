//! Mask keep mode by per-window capture ("toplevel" capture).
//!
//! Each kept window that the compositor IPC reports drawn on the captured
//! output is captured on its own - its own surface tree, nothing drawn above
//! it: no layer-shell surfaces, no other windows, no cursor - and drawn back to
//! front at its IPC rect onto a #1E1E1E canvas the size of the output image
//! (see `crate::mask` for the plan). Two protocols render one window into a
//! client buffer:
//!
//! - `ext_image_copy_capture_manager_v1` + `ext_foreign_toplevel_image_capture_source_manager_v1`
//!   (staging; Hyprland 0.54+; sway 1.12+ only behind the override), source =
//!   an `ext_foreign_toplevel_handle_v1`;
//! - `hyprland_toplevel_export_manager_v1` v2 (Hyprland, upright outputs),
//!   source = the `zwlr_foreign_toplevel_handle_v1` the `windows` ids come from.
//!
//! Otherwise (and always on sway) keep mode captures the composited output
//! and cuts (`crate::capture`). [`select`] makes the choice;
//! `WLROOTS_BRIDGE_MASK_CAPTURE` (`ext` | `hyprland` | `output`) forces one of
//! the paths the compositor supports, for testing.
//!
//! The capture handle of a kept IPC window: by Hyprland window address where
//! `hyprland_toplevel_mapping_v1` is advertised, by sway's
//! `foreign_toplevel_identifier` (ext path), else by app id + title, which must
//! then name exactly one handle and one IPC window. Anything else refuses.
//!
//! Hyprland delivers the window at its size times the output scale, without
//! border, shadow, rounding, blur or compositor opacity; xdg_popups inside the
//! window are included, popups past its edge are clipped
//! (`popups_outside_window`). A buffer that does not match makes the whole
//! call fall back to the output cut. Override-redirect XWayland windows
//! (menus, dropdowns) of a drawn X11 client are exported by window address and
//! drawn on top when they can be attributed (see `crate::mask`).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1, ext_image_capture_source_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};

use crate::ax::IpcKind;
use crate::capture::{BufferFormat, RgbImage, ShmBuffer, wait_until};
use crate::conn::{Conn, Globals};
use crate::ipc::{self, Client, Snapshot};
use crate::mask::{self, CaptureMode, DrawnChild, MaskReport, Plan, PlanContext};
use crate::output::Rect;
use crate::protocols::hyprland_toplevel_export::{
    hyprland_toplevel_export_frame_v1 as hframe, hyprland_toplevel_export_manager_v1 as hmgr,
};
use crate::protocols::hyprland_toplevel_mapping::{
    hyprland_toplevel_mapping_manager_v1 as mapmgr,
    hyprland_toplevel_window_mapping_handle_v1 as maphandle,
};
use crate::screens::OutputGeometry;
use crate::transform::Transform;
use crate::wm::Toplevel;

/// Forces a keep-mode capture path (testing aid, not in `--help`).
pub const OVERRIDE_ENV: &str = "WLROOTS_BRIDGE_MASK_CAPTURE";

/// Wall-clock budget for every window frame of one masked capture, both
/// attempts together. Hyprland with `ecosystem:enforce_permissions` holds the
/// frames of a client whose screencopy permission is still pending: that ends
/// here, as an error.
pub const CAPTURE_BUDGET: Duration = Duration::from_millis(4000);
/// A window's buffer size changed during the capture (ext
/// `failed(buffer_constraints)`): treated like a mismatch, the settle loop
/// waits and tries again.
#[derive(Debug)]
struct SizeChanged;

impl std::fmt::Display for SizeChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the window's buffer size changed during the capture")
    }
}

impl std::error::Error for SizeChanged {}

/// Pause between attempts while a window buffer does not match its rect.
pub const SETTLE_STEP: Duration = Duration::from_millis(80);
/// How long to wait for a window to settle after the first mismatch.
/// Hyprland animation speed is in 100 ms units; Omarchy's longest window
/// animation is `windowsIn` 4.1 (410 ms, easeOutQuint), `windows` 3.79
/// (379 ms). 800 ms covers that plus a capture round and slack.
pub const SETTLE_MAX: Duration = Duration::from_millis(800);

/// The settle loop's decision: wait and try again while the window may still
/// be animating, within [`SETTLE_MAX`] from the first mismatch and the call's
/// capture deadline.
#[derive(Debug)]
pub struct Settle {
    start: Option<Instant>,
    /// Retries granted so far.
    pub tries: u32,
}

impl Settle {
    pub fn new() -> Self {
        Settle {
            start: None,
            tries: 0,
        }
    }

    /// After a mismatch at `now`: `Some(pause)` to try again, `None` to give
    /// up (output cut).
    pub fn next(&mut self, now: Instant, deadline: Instant) -> Option<Duration> {
        let start = *self.start.get_or_insert(now);
        let end = (start + SETTLE_MAX).min(deadline);
        if now + SETTLE_STEP >= end {
            return None;
        }
        self.tries += 1;
        Some(SETTLE_STEP)
    }
}

/// Budget for one compositor IPC snapshot.
const IPC_DEADLINE: Duration = Duration::from_millis(1500);
/// Upper bound for a window buffer side (sanity, not a policy).
const MAX_SIDE: u32 = 32_768;

/// How a keep-mode screenshot is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// Per window, `ext_image_copy_capture_v1`.
    Ext,
    /// Per window, `hyprland_toplevel_export_v1`.
    Hyprland,
    /// The composited output, cut (`crate::capture`).
    Output,
}

impl Path {
    pub fn mode(self) -> CaptureMode {
        match self {
            Path::Ext | Path::Hyprland => CaptureMode::Toplevel,
            Path::Output => CaptureMode::Output,
        }
    }

    /// `doctor.screenshot_mask_protocol`.
    pub fn protocol(self) -> Option<&'static str> {
        match self {
            Path::Ext => Some("ext-image-copy-capture-v1"),
            Path::Hyprland => Some("hyprland-toplevel-export-v1"),
            Path::Output => None,
        }
    }
}

/// What the compositor advertises for per-window capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Avail {
    /// ext image copy capture of a foreign toplevel (+ the ext list for the
    /// handles, the wlr manager for the ids, wl_shm).
    pub ext: bool,
    /// `hyprland_toplevel_export_manager_v1` v2 (+ wlr manager, wl_shm).
    pub hyprland: bool,
    /// `hyprland_toplevel_export_manager_v1` at any version: capture by window
    /// address (override-redirect XWayland windows).
    pub hyprland_by_address: bool,
    /// `hyprland_toplevel_mapping_manager_v1`.
    pub mapping: bool,
}

impl Avail {
    pub fn from_globals(g: &Globals) -> Self {
        let base = g.shm.is_some() && g.foreign_toplevel_wlr.is_some();
        Avail {
            ext: base
                && g.image_copy_capture.is_some()
                && g.toplevel_capture_source.is_some()
                && g.foreign_toplevel_ext.is_some(),
            hyprland: base && g.hyprland_toplevel_export.is_some_and(|b| b.version >= 2),
            hyprland_by_address: g.shm.is_some() && g.hyprland_toplevel_export.is_some(),
            mapping: g.foreign_toplevel_wlr.is_some() && g.hyprland_toplevel_mapping.is_some(),
        }
    }
}

/// Pick the keep-mode path. Hyprland: ext, else the Hyprland export (upright
/// outputs only: its buffer carries no transform), else the output cut.
/// sway: the output cut (ext only through the override).
/// `forced` (from [`OVERRIDE_ENV`]) may pick any path the compositor supports
/// for this output; `output` is always allowed.
pub fn select(avail: Avail, kind: IpcKind, forced: Option<&str>, upright: bool) -> Result<Path> {
    let hyprland = avail.hyprland && kind == IpcKind::Hyprland && upright;
    match forced.map(str::trim).filter(|s| !s.is_empty()) {
        // sway: the output cut by default. Its ext toplevel source includes
        // client-side shadows and popups without a geometry offset and
        // renders at scale 1, so it stays behind the override.
        None if kind == IpcKind::Sway => Ok(Path::Output),
        None => Ok(if avail.ext {
            Path::Ext
        } else if hyprland {
            Path::Hyprland
        } else {
            Path::Output
        }),
        Some("output") => Ok(Path::Output),
        Some("ext") if avail.ext => Ok(Path::Ext),
        Some("hyprland") if hyprland => Ok(Path::Hyprland),
        Some(p @ ("ext" | "hyprland")) => {
            bail!(
                "{OVERRIDE_ENV}={p}: this compositor (or this output's transform) does not support that capture path"
            )
        }
        Some(other) => bail!("{OVERRIDE_ENV}={other}: expected ext, hyprland or output"),
    }
}

/// [`select`] with the override from the environment.
pub fn select_env(avail: Avail, kind: IpcKind, upright: bool) -> Result<Path> {
    select(
        avail,
        kind,
        std::env::var(OVERRIDE_ENV).ok().as_deref(),
        upright,
    )
}

/// `doctor`'s `(screenshot_mask_mode, screenshot_mask_protocol,
/// screenshot_mask_gaps)` on a compositor the bridge can mask on, for an
/// upright output. An override this compositor cannot honour (or a typo)
/// gives null mode and protocol and the default path's gaps; it never turns
/// the mask off (a masked capture then fails with the override error).
pub fn doctor_mask_fields(
    avail: Avail,
    kind: IpcKind,
    forced: Option<&str>,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Vec<&'static str>,
) {
    match select(avail, kind, forced, true) {
        Ok(path) => (
            Some(path.mode().as_str()),
            path.protocol(),
            gaps(path, avail, kind),
        ),
        Err(_) => {
            let default = select(avail, kind, None, true).unwrap_or(Path::Output);
            (None, None, gaps(default, avail, kind))
        }
    }
}

/// `doctor.screenshot_mask_gaps` for a path.
pub fn gaps(path: Path, avail: Avail, kind: IpcKind) -> Vec<&'static str> {
    match path {
        Path::Output => ipc::mask_gaps(kind).to_vec(),
        Path::Ext | Path::Hyprland => {
            let mut g = vec!["popups_outside_window"];
            if !(kind == IpcKind::Hyprland && avail.hyprland_by_address) {
                g.push("xwayland_unmanaged");
            }
            g
        }
    }
}

// ------------------------------------------------------------------ state

struct WlrEntry {
    handle: zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    toplevel: Toplevel,
    closed: bool,
}

struct ExtEntry {
    handle: ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    app_id: Option<String>,
    title: Option<String>,
    identifier: Option<String>,
    address: Option<u64>,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapping {
    Pending,
    Address(u64),
    Failed,
}

/// One frame in flight (either protocol).
#[derive(Default)]
struct Frame {
    // hyprland export: the offered shm buffer (raw format, w, h, stride).
    hypr_shm: Option<(u32, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    // ext session constraints.
    size: Option<(u32, u32)>,
    shm_formats: Vec<u32>,
    session_done: bool,
    session_stopped: bool,
    transform: Option<u32>,
    // both
    ready: bool,
    failed: Option<String>,
    /// ext: the frame failed because the buffer size changed (a window
    /// resizing or animating).
    constraints_changed: bool,
}

struct St {
    wlr: Vec<WlrEntry>,
    ext: Vec<ExtEntry>,
    maps: Vec<Mapping>,
    f: Frame,
}

impl St {
    fn wlr_index(
        &mut self,
        h: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
    ) -> usize {
        if let Some(i) = self.wlr.iter().position(|e| e.handle.id() == h.id()) {
            return i;
        }
        // Same id scheme as `windows` (`crate::wm`).
        let toplevel = Toplevel::with_id(format!("{}", h.id().protocol_id()));
        self.wlr.push(WlrEntry {
            handle: h.clone(),
            toplevel,
            closed: false,
        });
        self.wlr.len() - 1
    }

    fn ext_index(
        &mut self,
        h: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) -> usize {
        if let Some(i) = self.ext.iter().position(|e| e.handle.id() == h.id()) {
            return i;
        }
        self.ext.push(ExtEntry {
            handle: h.clone(),
            app_id: None,
            title: None,
            identifier: None,
            address: None,
            closed: false,
        });
        self.ext.len() - 1
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for St {
    fn event(
        s: &mut Self,
        _: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        e: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = e {
            s.wlr_index(&toplevel);
        }
    }

    wayland_client::event_created_child!(St, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for St {
    fn event(
        s: &mut Self,
        h: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        e: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let i = s.wlr_index(h);
        let t = &mut s.wlr[i].toplevel;
        match e {
            Event::AppId { app_id } => t.app_id = Some(app_id),
            Event::Title { title } => t.title = Some(title),
            Event::State { state } => {
                // wlr state enum: 1 = minimized.
                t.minimized = state
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|c| u32::from_ne_bytes(*c) == 1);
            }
            Event::Closed => s.wlr[i].closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for St {
    fn event(
        s: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        e: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = e {
            s.ext_index(&toplevel);
        }
    }

    wayland_client::event_created_child!(St, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for St {
    fn event(
        s: &mut Self,
        h: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        e: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        let i = s.ext_index(h);
        let x = &mut s.ext[i];
        match e {
            Event::AppId { app_id } => x.app_id = Some(app_id),
            Event::Title { title } => x.title = Some(title),
            Event::Identifier { identifier } => {
                x.identifier = Some(identifier).filter(|s| !s.is_empty())
            }
            Event::Closed => x.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<maphandle::HyprlandToplevelWindowMappingHandleV1, usize> for St {
    fn event(
        s: &mut Self,
        _: &maphandle::HyprlandToplevelWindowMappingHandleV1,
        e: maphandle::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let m = match e {
            maphandle::Event::WindowAddress {
                address_hi,
                address,
            } => {
                let a = (u64::from(address_hi) << 32) | u64::from(address);
                if a == 0 {
                    Mapping::Failed
                } else {
                    Mapping::Address(a)
                }
            }
            maphandle::Event::Failed => Mapping::Failed,
        };
        if let Some(slot) = s.maps.get_mut(*idx) {
            *slot = m;
        }
    }
}

fn raw_format(f: WEnum<wl_shm::Format>) -> u32 {
    match f {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(v) => v,
    }
}

impl Dispatch<hframe::HyprlandToplevelExportFrameV1, ()> for St {
    fn event(
        s: &mut Self,
        _: &hframe::HyprlandToplevelExportFrameV1,
        e: hframe::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            hframe::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => s.f.hypr_shm = Some((raw_format(format), width, height, stride)),
            hframe::Event::BufferDone => s.f.buffer_done = true,
            hframe::Event::Flags { flags } => {
                s.f.y_invert = match flags {
                    WEnum::Value(fl) => fl.contains(hframe::Flags::YInvert),
                    WEnum::Unknown(raw) => raw & 1 != 0,
                }
            }
            hframe::Event::Ready { .. } => s.f.ready = true,
            hframe::Event::Failed => s.f.failed = Some("failed".into()),
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for St {
    fn event(
        s: &mut Self,
        _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        e: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match e {
            Event::BufferSize { width, height } => {
                // A later `done` may follow new constraints; the first set
                // is what the frame is allocated for.
                if !s.f.session_done {
                    s.f.size = Some((width, height));
                }
            }
            Event::ShmFormat { format } => {
                if !s.f.session_done {
                    s.f.shm_formats.push(raw_format(format));
                }
            }
            Event::Done => s.f.session_done = true,
            Event::Stopped => s.f.session_stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for St {
    fn event(
        s: &mut Self,
        _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        e: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match e {
            Event::Transform { transform } => {
                s.f.transform = Some(match transform {
                    WEnum::Value(v) => v.into(),
                    WEnum::Unknown(v) => v,
                })
            }
            Event::Ready => s.f.ready = true,
            Event::Failed { reason } => {
                s.f.constraints_changed = matches!(
                    reason,
                    WEnum::Value(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints)
                );
                s.f.failed = Some(format!("{reason:?}"));
            }
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(St: ignore hmgr::HyprlandToplevelExportManagerV1);
wayland_client::delegate_noop!(St: ignore mapmgr::HyprlandToplevelMappingManagerV1);
wayland_client::delegate_noop!(St: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(St: ignore ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(St: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(St: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(St: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(St: ignore wl_buffer::WlBuffer);

// ---------------------------------------------------------------- session

/// The toplevel handles of one masked capture, on their own event queue.
pub struct Session<'c> {
    conn: &'c Conn,
    q: EventQueue<St>,
    st: St,
    mapping: bool,
    /// wlr handles known when the session opened ([`Session::handles_stable`]).
    initial_wlr: usize,
}

impl<'c> Session<'c> {
    /// Bind the wlr manager FIRST, so the handles get the same object ids as
    /// in `windows` (the ids are per-connection protocol ids, assigned in
    /// creation order); then, if asked, the ext list; then map every handle
    /// to its Hyprland window address when the compositor can.
    pub fn open(conn: &'c Conn, with_ext: bool, avail: Avail) -> Result<Self> {
        let g = conn
            .globals
            .foreign_toplevel_wlr
            .context("compositor does not advertise zwlr_foreign_toplevel_manager_v1")?;
        let mut q = conn.conn.new_event_queue::<St>();
        let qh = q.handle();
        let _wlr: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 =
            conn.registry.bind(g.name, g.version.min(3), &qh, ());
        let mut st = St {
            wlr: Vec::new(),
            ext: Vec::new(),
            maps: Vec::new(),
            f: Frame::default(),
        };
        // The first roundtrip delivers the handles, the second their state.
        q.roundtrip(&mut st)?;
        q.roundtrip(&mut st)?;
        if with_ext {
            let g = conn
                .globals
                .foreign_toplevel_ext
                .context("compositor does not advertise ext_foreign_toplevel_list_v1")?;
            let _list: ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1 =
                conn.bind(g, 1, &qh, ());
            q.roundtrip(&mut st)?;
            q.roundtrip(&mut st)?;
        }
        let initial_wlr = st.wlr.len();
        let mut s = Session {
            conn,
            q,
            st,
            mapping: false,
            initial_wlr,
        };
        if avail.mapping {
            s.map_addresses()?;
        }
        Ok(s)
    }

    fn map_addresses(&mut self) -> Result<()> {
        let g = self
            .conn
            .globals
            .hyprland_toplevel_mapping
            .context("no hyprland_toplevel_mapping_manager_v1")?;
        let qh = self.q.handle();
        let mgr: mapmgr::HyprlandToplevelMappingManagerV1 = self.conn.bind(g, 1, &qh, ());
        let mut wlr_maps = Vec::new();
        for (i, e) in self.st.wlr.iter().enumerate() {
            let idx = self.st.maps.len();
            self.st.maps.push(Mapping::Pending);
            let h = mgr.get_window_for_toplevel_wlr(&e.handle, &qh, idx);
            wlr_maps.push((i, idx, h));
        }
        let mut ext_maps = Vec::new();
        for (i, e) in self.st.ext.iter().enumerate() {
            let idx = self.st.maps.len();
            self.st.maps.push(Mapping::Pending);
            let h = mgr.get_window_for_toplevel(&e.handle, &qh, idx);
            ext_maps.push((i, idx, h));
        }
        self.q.roundtrip(&mut self.st)?;
        for (i, idx, h) in wlr_maps {
            if let Mapping::Address(a) = self.st.maps[idx] {
                self.st.wlr[i].toplevel.address = Some(a);
            }
            h.destroy();
        }
        for (i, idx, h) in ext_maps {
            if let Mapping::Address(a) = self.st.maps[idx] {
                self.st.ext[i].address = Some(a);
            }
            h.destroy();
        }
        mgr.destroy();
        self.mapping = true;
        Ok(())
    }

    /// Whether kept windows are located by window address.
    pub fn by_address(&self) -> bool {
        self.mapping
    }

    /// The open toplevels, as `windows` lists them (address set when mapped).
    pub fn toplevels(&self) -> Vec<Toplevel> {
        self.st
            .wlr
            .iter()
            .filter(|e| !e.closed)
            .map(|e| e.toplevel.clone())
            .collect()
    }

    fn wait(&mut self, deadline: Instant, what: &str, done: impl Fn(&St) -> bool) -> Result<()> {
        let started = Instant::now();
        if !wait_until(&self.conn.conn, &mut self.q, &mut self.st, deadline, done)? {
            bail!(
                "{what}: no answer from the compositor within the {} ms capture budget (waited {} ms; \
                 Hyprland holds the frames of a client whose screencopy permission is pending under \
                 ecosystem:enforce_permissions)",
                CAPTURE_BUDGET.as_millis(),
                started.elapsed().as_millis()
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_ext(
        &mut self,
        icc: &ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
        src: &ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
        shm: &wl_shm::WlShm,
        ext_idx: usize,
        limit: u32,
        deadline: Instant,
    ) -> Result<WinImage> {
        let qh = self.q.handle();
        self.st.f = Frame::default();
        let handle = self.st.ext[ext_idx].handle.clone();
        let source = src.create_source(&handle, &qh, ());
        let session = icc.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            (),
        );
        let mut frame: Option<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1> = None;
        let mut buf: Option<ShmBuffer> = None;
        let result = (|| {
            self.wait(deadline, "window capture session", |s| {
                s.f.session_done || s.f.session_stopped || s.ext[ext_idx].closed
            })?;
            if self.st.f.session_stopped || self.st.ext[ext_idx].closed {
                bail!("the compositor stopped the window capture (window closed?)");
            }
            let (w, h) = self
                .st
                .f
                .size
                .context("window capture session sent no buffer size")?;
            let Some(format) = pick_format(&self.st.f.shm_formats) else {
                bail!(
                    "unsupported window buffer format(s) {}: only 8-bit RGB shm formats are read",
                    fmt_list(&self.st.f.shm_formats)
                );
            };
            let stride = check_buffer(w, h, w.saturating_mul(4), limit)?;
            let fmt = BufferFormat {
                format,
                width: w,
                height: h,
                stride,
            };
            let b = buf.insert(ShmBuffer::new(shm, &qh, fmt)?);
            let f = frame.insert(session.create_frame(&qh, ()));
            f.attach_buffer(&b.buffer);
            f.damage_buffer(0, 0, w as i32, h as i32);
            f.capture();
            self.wait(deadline, "window frame", |s| {
                s.f.ready || s.f.failed.is_some() || s.f.session_stopped
            })?;
            if !self.st.f.ready && self.st.f.constraints_changed {
                return Err(anyhow::Error::new(SizeChanged));
            }
            if !self.st.f.ready {
                bail!(
                    "window frame failed ({})",
                    self.st.f.failed.as_deref().unwrap_or("session stopped")
                );
            }
            let img = decode_rgba(b.mmap.as_slice(), fmt, false)?;
            let t = Transform::from_raw(self.st.f.transform.unwrap_or(0));
            Ok(transform_rgba(img, t))
        })();
        // Every protocol object goes, on every path.
        if let Some(f) = &frame {
            f.destroy();
        }
        session.destroy();
        source.destroy();
        if let Some(b) = &buf {
            b.destroy();
        }
        result
    }

    fn capture_hypr(
        &mut self,
        shm: &wl_shm::WlShm,
        limit: u32,
        deadline: Instant,
        make: impl FnOnce(&QueueHandle<St>) -> hframe::HyprlandToplevelExportFrameV1,
    ) -> Result<WinImage> {
        let qh = self.q.handle();
        self.st.f = Frame::default();
        let frame = make(&qh);
        let mut buf: Option<ShmBuffer> = None;
        let result = (|| {
            self.wait(deadline, "window export", |s| {
                s.f.buffer_done || s.f.failed.is_some()
            })?;
            if self.st.f.failed.is_some() {
                bail!("the compositor refused the window export (window closed?)");
            }
            let Some((raw, w, h, stride)) = self.st.f.hypr_shm else {
                bail!("the window export offered no shm buffer");
            };
            let Some(format) = pick_format(&[raw]) else {
                bail!(
                    "unsupported window buffer format {}: only 8-bit RGB shm formats are read",
                    fmt_list(&[raw])
                );
            };
            let stride = check_buffer(w, h, stride, limit)?;
            let fmt = BufferFormat {
                format,
                width: w,
                height: h,
                stride,
            };
            let b = buf.insert(ShmBuffer::new(shm, &qh, fmt)?);
            frame.copy(&b.buffer, 1);
            self.wait(deadline, "window export copy", |s| {
                s.f.ready || s.f.failed.is_some()
            })?;
            if !self.st.f.ready {
                bail!("the window export copy failed (window closed?)");
            }
            decode_rgba(b.mmap.as_slice(), fmt, self.st.f.y_invert)
        })();
        frame.destroy();
        if let Some(b) = &buf {
            b.destroy();
        }
        result
    }

    /// Whether the handle list still tells toplevels apart from other
    /// windows: no toplevel appeared since the session opened and, when
    /// windows are located by address, every open handle has one. Call after
    /// [`Session::check_kept_open`] (which dispatches new handles).
    pub fn handles_stable(&self) -> bool {
        handles_stable(
            self.initial_wlr,
            self.mapping,
            &self
                .st
                .wlr
                .iter()
                .map(|e| (e.closed, e.toplevel.address.is_some()))
                .collect::<Vec<_>>(),
        )
    }

    /// Dispatch what has arrived and refuse when a kept toplevel has closed:
    /// its address may already belong to another window.
    pub fn check_kept_open(&mut self, ids: &[String]) -> Result<()> {
        self.q.roundtrip(&mut self.st)?;
        if let Some(e) = self
            .st
            .wlr
            .iter()
            .find(|e| e.closed && ids.contains(&e.toplevel.id))
        {
            bail!(
                "window `{}` closed while capturing; masked capture refused",
                e.toplevel.id
            );
        }
        Ok(())
    }
}

/// See [`Session::handles_stable`]: `handles` = `(closed, has_address)`.
fn handles_stable(initial: usize, by_address: bool, handles: &[(bool, bool)]) -> bool {
    handles.len() == initial && (!by_address || handles.iter().all(|(closed, a)| *closed || *a))
}

/// Slack a compositor may add to a row (alignment padding), in bytes.
const STRIDE_PAD: u32 = 1024;

/// Validate a window buffer before it is allocated: both sides positive and
/// at most `limit`, a stride of at least `4 * w` and at most `4 * w` plus
/// [`STRIDE_PAD`], and `stride * h` within a wl_shm pool (`i32`). Returns the
/// stride.
fn check_buffer(w: u32, h: u32, stride: u32, limit: u32) -> Result<u32> {
    anyhow::ensure!(
        w > 0 && h > 0 && w <= limit && h <= limit,
        "window buffer size {w}x{h} is out of range (limit {limit} per side)"
    );
    let min = u64::from(w) * 4;
    anyhow::ensure!(
        u64::from(stride) >= min && u64::from(stride) <= min + u64::from(STRIDE_PAD),
        "window buffer stride {stride} does not fit width {w}"
    );
    anyhow::ensure!(
        u64::from(stride) * u64::from(h) <= i32::MAX as u64,
        "window buffer {w}x{h} (stride {stride}) is too large"
    );
    Ok(stride)
}

/// The per-side buffer limit for an output image of `iw x ih`: four times
/// its long side (a window may extend past the output), at most [`MAX_SIDE`].
fn side_limit(iw: u32, ih: u32) -> u32 {
    iw.max(ih).saturating_mul(4).min(MAX_SIDE)
}

fn fmt_list(f: &[u32]) -> String {
    if f.is_empty() {
        return "(none)".into();
    }
    f.iter()
        .map(|v| format!("0x{v:08x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first 8-bit RGB format we read, by preference (opaque first).
fn pick_format(offered: &[u32]) -> Option<wl_shm::Format> {
    [
        wl_shm::Format::Xrgb8888,
        wl_shm::Format::Argb8888,
        wl_shm::Format::Xbgr8888,
        wl_shm::Format::Abgr8888,
    ]
    .into_iter()
    .find(|f| offered.contains(&u32::from(*f)))
}

// --------------------------------------------------------------- pixels

/// A captured window: premultiplied RGBA, row-major, no padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinImage {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

fn decode_rgba(data: &[u8], fmt: BufferFormat, y_invert: bool) -> Result<WinImage> {
    let (ro, go, bo, alpha) = match fmt.format {
        wl_shm::Format::Argb8888 => (2, 1, 0, true),
        wl_shm::Format::Xrgb8888 => (2, 1, 0, false),
        wl_shm::Format::Abgr8888 => (0, 1, 2, true),
        wl_shm::Format::Xbgr8888 => (0, 1, 2, false),
        other => bail!("unsupported window buffer format {other:?}"),
    };
    let (w, h, st) = (fmt.width as usize, fmt.height as usize, fmt.stride as usize);
    let need = st.checked_mul(h).context("window buffer size overflows")?;
    anyhow::ensure!(
        data.len() >= need && st >= w * 4,
        "window buffer is {} bytes, expected {need}",
        data.len()
    );
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        let sy = if y_invert { h - 1 - y } else { y };
        let src = &data[sy * st..sy * st + w * 4];
        let dst = &mut px[y * w * 4..(y + 1) * w * 4];
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            d[0] = s[ro];
            d[1] = s[go];
            d[2] = s[bo];
            d[3] = if alpha { s[3] } else { 255 };
        }
    }
    Ok(WinImage {
        w: fmt.width,
        h: fmt.height,
        px,
    })
}

/// Re-orient a buffer-oriented window image (ext frame `transform`).
fn transform_rgba(src: WinImage, t: Transform) -> WinImage {
    if t == Transform::Normal {
        return src;
    }
    let (lw, lh) = t.transformed_size(src.w, src.h);
    let mut px = vec![0u8; src.px.len()];
    for by in 0..src.h {
        for bx in 0..src.w {
            let (lx, ly) = t.buffer_to_logical(bx, by, src.w, src.h);
            let s = ((by * src.w + bx) as usize) * 4;
            let d = ((ly * lw + lx) as usize) * 4;
            px[d..d + 4].copy_from_slice(&src.px[s..s + 4]);
        }
    }
    WinImage { w: lw, h: lh, px }
}

/// Where a window buffer lands on the canvas, in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x0: i64,
    pub y0: i64,
    pub w: i64,
    pub h: i64,
    /// The buffer does not match the rect's pixel size: scaled to fit.
    pub resample: bool,
}

fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}
fn ceil_div(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}
/// `a / b` rounded half up (b > 0).
fn round_div(a: i64, b: i64) -> i64 {
    (2 * a + b).div_euclid(2 * b)
}

/// Place a `bw x bh` window buffer for the logical `rect` on an `iw x ih`
/// image of the logical area `output`. The origin is the rect's origin scaled
/// and rounded half up (as the compositor rounds a scaled box). The size is
/// the BUFFER's when it lies between the rect's inward- and outward-rounded
/// pixel size (one pixel of slack each way), so a fractional scale draws the
/// window 1:1 without resampling; otherwise the buffer is scaled into the
/// outward-rounded rect (a window resized during the capture).
pub fn place(rect: &Rect, output: &Rect, iw: u32, ih: u32, bw: u32, bh: u32) -> Placement {
    let axis = |pos: i32, len: i32, opos: i32, olen: i32, img: u32, buf: u32| {
        let (olen, img) = (i64::from(olen.max(1)), i64::from(img));
        let l0 = i64::from(pos) - i64::from(opos);
        let l1 = l0 + i64::from(len);
        let start = round_div(l0 * img, olen);
        let (out0, out1) = (floor_div(l0 * img, olen), ceil_div(l1 * img, olen));
        let (in0, in1) = (ceil_div(l0 * img, olen), floor_div(l1 * img, olen));
        let b = i64::from(buf);
        let fits = b >= (in1 - in0) - 1 && b <= (out1 - out0) + 1;
        (start, b, out0, out1 - out0, fits)
    };
    let (x, bwi, ox, ow, fx) = axis(rect.x, rect.width, output.x, output.width, iw, bw);
    let (y, bhi, oy, oh, fy) = axis(rect.y, rect.height, output.y, output.height, ih, bh);
    if fx && fy {
        Placement {
            x0: x,
            y0: y,
            w: bwi,
            h: bhi,
            resample: false,
        }
    } else {
        Placement {
            x0: ox,
            y0: oy,
            w: ow,
            h: oh,
            resample: true,
        }
    }
}

/// Draw `img` at `p` onto `canvas`, alpha (premultiplied) over what is there.
pub fn blit(canvas: &mut RgbImage, img: &WinImage, p: &Placement) {
    if p.w <= 0 || p.h <= 0 || img.w == 0 || img.h == 0 {
        return;
    }
    let (cw, ch) = (i64::from(canvas.width), i64::from(canvas.height));
    let (bw, bh) = (i64::from(img.w), i64::from(img.h));
    for y in p.y0.max(0)..(p.y0 + p.h).min(ch) {
        let sy = if p.resample {
            ((y - p.y0) * bh / p.h).min(bh - 1)
        } else {
            y - p.y0
        };
        if sy >= bh {
            continue;
        }
        for x in p.x0.max(0)..(p.x0 + p.w).min(cw) {
            let sx = if p.resample {
                ((x - p.x0) * bw / p.w).min(bw - 1)
            } else {
                x - p.x0
            };
            if sx >= bw {
                continue;
            }
            let s = &img.px[((sy * bw + sx) * 4) as usize..][..4];
            let d = &mut canvas.pixels[((y * cw + x) * 3) as usize..][..3];
            let a = u32::from(s[3]);
            for c in 0..3 {
                let v = u32::from(s[c]) + (u32::from(d[c]) * (255 - a) + 127) / 255;
                d[c] = v.min(255) as u8;
            }
        }
    }
}

// ------------------------------------------------------------- capture

/// Which handle captures an IPC window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Ext(usize),
    Wlr(usize),
}

fn key_count(snap: &Snapshot, c: &Client) -> usize {
    snap.clients
        .iter()
        .filter(|o| o.app_id == c.app_id && o.title == c.title)
        .count()
}

/// A capture handle as far as picking one goes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Cand {
    closed: bool,
    address: Option<u64>,
    identifier: Option<String>,
    app_id: String,
    title: String,
}

/// The handle that captures the kept IPC window `c`: by window address when
/// the handles are mapped, else by sway's `foreign_toplevel_identifier` (ext
/// handles carry it), else by app id + title when that pair is unique among
/// the IPC windows. Exactly one open handle must match.
fn pick_source(cands: &[Cand], snap: &Snapshot, c: &Client, mapped: bool) -> Result<usize> {
    let open = || cands.iter().enumerate().filter(|(_, e)| !e.closed);
    let hits: Vec<usize> = if mapped {
        match c.address {
            Some(a) => open()
                .filter(|(_, e)| e.address == Some(a))
                .map(|(i, _)| i)
                .collect(),
            None => Vec::new(),
        }
    } else if c.identifier.is_some() && cands.iter().any(|e| e.identifier.is_some()) {
        open()
            .filter(|(_, e)| e.identifier == c.identifier)
            .map(|(i, _)| i)
            .collect()
    } else if key_count(snap, c) == 1 {
        open()
            .filter(|(_, e)| e.app_id == c.app_id && e.title == c.title)
            .map(|(i, _)| i)
            .collect()
    } else {
        Vec::new()
    };
    match hits.as_slice() {
        [i] => Ok(*i),
        _ => bail!(
            "no unambiguous capture handle for window {} ({} candidates); masked capture refused",
            describe(c),
            hits.len()
        ),
    }
}

impl St {
    fn ext_cands(&self) -> Vec<Cand> {
        self.ext
            .iter()
            .map(|e| Cand {
                closed: e.closed,
                address: e.address,
                identifier: e.identifier.clone(),
                app_id: e.app_id.clone().unwrap_or_default(),
                title: e.title.clone().unwrap_or_default(),
            })
            .collect()
    }

    fn wlr_cands(&self) -> Vec<Cand> {
        self.wlr
            .iter()
            .map(|e| Cand {
                closed: e.closed,
                address: e.toplevel.address,
                identifier: None,
                app_id: e.toplevel.app_id.clone().unwrap_or_default(),
                title: e.toplevel.title.clone().unwrap_or_default(),
            })
            .collect()
    }
}

fn describe(c: &Client) -> String {
    if c.app_id.is_empty() {
        "(no app id)".to_owned()
    } else {
        format!("`{}`", c.app_id)
    }
}

/// The 32-bit export handle of a window address, refused when another window
/// shares its low 32 bits (the v1 request takes only those).
fn address_handle(snap: &Snapshot, addr: u64) -> Result<u32> {
    let low = (addr & 0xffff_ffff) as u32;
    let clash = snap
        .clients
        .iter()
        .filter_map(|c| c.address)
        .any(|a| a != addr && (a & 0xffff_ffff) as u32 == low);
    anyhow::ensure!(
        !clash,
        "two windows share the low 32 bits of address 0x{addr:x}; masked capture refused"
    );
    Ok(low)
}

/// One stderr line per drawn window when `WLROOTS_BRIDGE_MASK_DEBUG` is set
/// (testing aid; stdout stays the one JSON value).
fn debug(msg: &str) {
    if std::env::var_os("WLROOTS_BRIDGE_MASK_DEBUG").is_some() {
        eprintln!("[mask] {msg}");
    }
}

/// Keep mode by per-window capture: the canvas (output image size, logical
/// orientation) and the report.
pub fn capture_keep(
    conn: &Conn,
    sess: &mut Session<'_>,
    geom: &OutputGeometry,
    output: &Rect,
    ids: &[String],
    path: Path,
) -> Result<Option<(RgbImage, MaskReport)>> {
    anyhow::ensure!(
        path != Path::Output,
        "per-window capture needs ext or hyprland"
    );
    let avail = Avail::from_globals(&conn.globals);
    let (iw, ih) = geom
        .transformed_mode()
        .filter(|(w, h)| *w > 0 && *h > 0)
        .context("the output reports no mode size")?;
    let (iw, ih) = (iw as u32, ih as u32);

    let toplevels = sess.toplevels();
    let by_address = sess.by_address();
    let limit = side_limit(iw, ih);
    // A buffer that does not match its window's rect x scale means the
    // geometry assumption is broken: resampling would put pixels where
    // clicks do not land. Only the overridden sway ext path (scale-1
    // buffers) resamples; elsewhere the call falls back to the output cut.
    let may_resample = ipc::kind() == Some(IpcKind::Sway);
    // Before every plan: a kept handle that closed refuses (its address may
    // already name another window).
    let read_plan = |sess: &mut Session<'_>| -> Result<(Plan, Snapshot)> {
        let t = Instant::now();
        sess.check_kept_open(ids)?;
        // A toplevel that appeared after the handle list was taken (or a
        // handle without an address) could pass as an override-redirect
        // child: no children in this attempt.
        let ctx = PlanContext {
            output,
            mode: CaptureMode::Toplevel,
            by_address,
            children_allowed: sess.handles_stable(),
        };
        let t_rt = t.elapsed().as_millis();
        let snap = ipc::snapshot(true, IPC_DEADLINE)
            .context("masked capture needs the compositor IPC (sway or Hyprland)")?;
        debug(&format!(
            "plan: roundtrip {t_rt} ms, IPC {} ms",
            t.elapsed().as_millis() - t_rt
        ));
        let plan = mask::plan(&toplevels, ids, &snap, ctx)?;
        Ok((plan, snap))
    };

    let qh = sess.q.handle();
    let shm = conn.bind_shm(&qh)?;
    let ext = if path == Path::Ext {
        let g1 = conn
            .globals
            .image_copy_capture
            .context("no ext_image_copy_capture_manager_v1")?;
        let g2 = conn
            .globals
            .toplevel_capture_source
            .context("no ext_foreign_toplevel_image_capture_source_manager_v1")?;
        let icc: ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 =
            conn.bind(g1, 1, &qh, ());
        let src: ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1 =
            conn.bind(g2, 1, &qh, ());
        Some((icc, src))
    } else {
        None
    };
    // The export manager: the Hyprland path, and override-redirect children
    // (by address) on either path.
    let hypr: Option<hmgr::HyprlandToplevelExportManagerV1> =
        match conn.globals.hyprland_toplevel_export {
            Some(g) if path == Path::Hyprland || avail.hyprland_by_address => {
                Some(conn.bind(g, 2, &qh, ()))
            }
            _ => None,
        };

    let call_start = Instant::now();
    let deadline = call_start + CAPTURE_BUDGET;
    let mut settle = Settle::new();
    let mut changed = 0;
    'attempt: loop {
        let (before, snap) = read_plan(sess)?;
        let mut images: Vec<(Placement, WinImage)> = Vec::new();
        let mut used: Vec<Source> = Vec::new();
        for c in &before.draws {
            let rect = c.draw_rect().context("kept window lost its geometry")?;
            let (img, source) = match (&ext, path) {
                (Some((icc, src)), Path::Ext) => {
                    let i = pick_source(&sess.st.ext_cands(), &snap, c, sess.by_address())?;
                    let img = match sess.capture_ext(icc, src, &shm, i, limit, deadline) {
                        Ok(img) => img,
                        Err(e) if e.is::<SizeChanged>() => {
                            let what =
                                format!("window {} changed size during the capture", describe(c));
                            match settle.next(Instant::now(), deadline) {
                                Some(pause) => {
                                    debug(&format!(
                                        "{what}; retry {} in {} ms",
                                        settle.tries,
                                        pause.as_millis()
                                    ));
                                    std::thread::sleep(pause);
                                    continue 'attempt;
                                }
                                None => {
                                    debug(&format!(
                                        "{what}; still after {} retries, output cut instead",
                                        settle.tries
                                    ));
                                    return Ok(None);
                                }
                            }
                        }
                        Err(e) => {
                            return Err(e.context(format!("capturing window {}", describe(c))));
                        }
                    };
                    (img, Source::Ext(i))
                }
                _ => {
                    let m = hypr
                        .as_ref()
                        .context("no hyprland_toplevel_export_manager_v1")?;
                    let i = pick_source(&sess.st.wlr_cands(), &snap, c, sess.by_address())?;
                    let handle = sess.st.wlr[i].handle.clone();
                    let img = sess
                        .capture_hypr(&shm, limit, deadline, |qh| {
                            m.capture_toplevel_with_wlr_toplevel_handle(0, &handle, qh, ())
                        })
                        .with_context(|| format!("capturing window {}", describe(c)))?;
                    (img, Source::Wlr(i))
                }
            };
            used.push(source);
            let p = place(&rect, output, iw, ih, img.w, img.h);
            if p.resample && !may_resample {
                let what = format!(
                    "window {} rect {rect:?} buffer {}x{} does not match its scaled rect",
                    describe(c),
                    img.w,
                    img.h
                );
                match settle.next(Instant::now(), deadline) {
                    Some(pause) => {
                        debug(&format!(
                            "{what}; retry {} in {} ms",
                            settle.tries,
                            pause.as_millis()
                        ));
                        std::thread::sleep(pause);
                        continue 'attempt;
                    }
                    None => {
                        debug(&format!(
                            "{what}; still after {} retries, output cut instead",
                            settle.tries
                        ));
                        return Ok(None);
                    }
                }
            }
            debug(&format!(
                "window {} rect {rect:?} buffer {}x{} -> {p:?} ({} ms into the call)",
                describe(c),
                img.w,
                img.h,
                call_start.elapsed().as_millis()
            ));
            images.push((p, img));
        }
        let mut children: Vec<DrawnChild> = Vec::new();
        let mut skipped = before.children_skipped;
        for (c, parent) in before.children.iter().zip(&before.child_parents) {
            let (Some(m), Some(rect), Some(addr), Some(pid)) =
                (&hypr, c.draw_rect(), c.address, c.pid)
            else {
                skipped += 1;
                continue;
            };
            {
                let low = address_handle(&snap, addr)?;
                let img = sess
                    .capture_hypr(&shm, limit, deadline, |qh| {
                        m.capture_toplevel(0, low, qh, ())
                    })
                    .context("capturing a menu or dropdown of a kept XWayland window")?;
                let p = place(&rect, output, iw, ih, img.w, img.h);
                if p.resample && !may_resample {
                    let what = format!(
                        "child 0x{addr:x} rect {rect:?} buffer {}x{} does not match its scaled rect",
                        img.w, img.h
                    );
                    match settle.next(Instant::now(), deadline) {
                        Some(pause) => {
                            debug(&format!(
                                "{what}; retry {} in {} ms",
                                settle.tries,
                                pause.as_millis()
                            ));
                            std::thread::sleep(pause);
                            continue 'attempt;
                        }
                        None => {
                            debug(&format!(
                                "{what}; still after {} retries, output cut instead",
                                settle.tries
                            ));
                            return Ok(None);
                        }
                    }
                }
                debug(&format!(
                    "child 0x{addr:x} rect {rect:?} buffer {}x{} -> {p:?}",
                    img.w, img.h
                ));
                images.push((p, img));
                children.push(DrawnChild {
                    address: format!("0x{addr:x}"),
                    pid,
                    parent: parent.clone(),
                    rect,
                });
            }
        }
        // Dispatch what arrived meanwhile (a `closed` among it), then check.
        sess.q.roundtrip(&mut sess.st)?;
        let closed = used.iter().any(|s| match *s {
            Source::Ext(i) => sess.st.ext[i].closed,
            Source::Wlr(i) => sess.st.wlr[i].closed,
        });
        anyhow::ensure!(
            !closed,
            "a kept window closed while capturing; masked capture refused"
        );
        let (after, _) = read_plan(sess)?;
        if !before.same_image(&after, CaptureMode::Toplevel) {
            // As before: one more attempt, then refuse.
            changed += 1;
            debug(&format!(
                "plan changed during the capture (attempt {changed})"
            ));
            if changed >= 2 {
                bail!("windows changed while capturing (twice); masked capture refused");
            }
            continue;
        }
        let mut canvas = RgbImage::mask_canvas(iw, ih);
        for (p, img) in &images {
            blit(&mut canvas, img, p);
        }
        // The drawn windows' areas, then the drawn children's.
        let mut regions: Vec<Rect> = after
            .draws
            .iter()
            .filter_map(|c| c.draw_rect().and_then(|r| mask::intersect(&r, output)))
            .collect();
        regions.extend(
            children
                .iter()
                .filter_map(|c| mask::intersect(&c.rect, output)),
        );
        let report = MaskReport {
            mode: "keep",
            capture: Some(CaptureMode::Toplevel.as_str()),
            kept: after.kept,
            regions,
            children: Some(children),
            children_skipped: Some(skipped),
        };
        if settle.tries > 0 {
            debug(&format!("settled after {} retries", settle.tries));
        }
        return Ok(Some((canvas, report)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::BoundGlobal;
    use crate::ipc::tests::r;

    fn g(version: u32) -> Option<BoundGlobal> {
        Some(BoundGlobal { name: 1, version })
    }

    fn hyprland_056() -> Globals {
        Globals {
            shm: g(1),
            foreign_toplevel_wlr: g(3),
            foreign_toplevel_ext: g(1),
            image_copy_capture: g(1),
            toplevel_capture_source: g(1),
            hyprland_toplevel_export: g(2),
            hyprland_toplevel_mapping: g(1),
            ..Default::default()
        }
    }

    fn sway_112() -> Globals {
        Globals {
            shm: g(1),
            foreign_toplevel_wlr: g(3),
            foreign_toplevel_ext: g(1),
            image_copy_capture: g(1),
            toplevel_capture_source: g(1),
            ..Default::default()
        }
    }

    #[test]
    fn selection_prefers_ext_then_hyprland_then_output() {
        let h = Avail::from_globals(&hyprland_056());
        assert!(h.ext && h.hyprland && h.mapping && h.hyprland_by_address);
        assert_eq!(select(h, IpcKind::Hyprland, None, true).unwrap(), Path::Ext);
        // Hyprland < 0.54: export v2 only.
        let mut old = hyprland_056();
        old.image_copy_capture = None;
        old.toplevel_capture_source = None;
        old.foreign_toplevel_ext = None;
        old.hyprland_toplevel_mapping = None;
        let o = Avail::from_globals(&old);
        assert_eq!(
            select(o, IpcKind::Hyprland, None, true).unwrap(),
            Path::Hyprland
        );
        // Export v1 only: no wlr-handle capture -> output cut.
        old.hyprland_toplevel_export = g(1);
        let o1 = Avail::from_globals(&old);
        assert!(!o1.hyprland && o1.hyprland_by_address);
        assert_eq!(
            select(o1, IpcKind::Hyprland, None, true).unwrap(),
            Path::Output
        );
        // sway 1.12: ext. sway < 1.12: output.
        let s = Avail::from_globals(&sway_112());
        assert!(!s.mapping && !s.hyprland);
        assert_eq!(
            select(s, IpcKind::Sway, None, true).unwrap(),
            Path::Output,
            "sway: output cut by default"
        );
        assert_eq!(
            select(s, IpcKind::Sway, Some("ext"), true).unwrap(),
            Path::Ext
        );
        let mut s111 = sway_112();
        s111.image_copy_capture = None;
        assert_eq!(
            select(Avail::from_globals(&s111), IpcKind::Sway, None, true).unwrap(),
            Path::Output
        );
        // A Hyprland export global seen under sway's IPC is not used.
        let mut odd = hyprland_056();
        odd.image_copy_capture = None;
        assert_eq!(
            select(Avail::from_globals(&odd), IpcKind::Sway, None, true).unwrap(),
            Path::Output
        );
        // Each ext piece is required, wl_shm and the wlr manager too.
        for strip in 0..5 {
            let mut x = sway_112();
            match strip {
                0 => x.toplevel_capture_source = None,
                1 => x.foreign_toplevel_ext = None,
                2 => x.shm = None,
                3 => x.foreign_toplevel_wlr = None,
                _ => x.image_copy_capture = None,
            }
            assert!(!Avail::from_globals(&x).ext, "case {strip}");
        }
    }

    #[test]
    fn override_selects_only_supported_paths() {
        let h = Avail::from_globals(&hyprland_056());
        assert_eq!(
            select(h, IpcKind::Hyprland, Some("hyprland"), true).unwrap(),
            Path::Hyprland
        );
        assert_eq!(
            select(h, IpcKind::Hyprland, Some("ext"), true).unwrap(),
            Path::Ext
        );
        assert_eq!(
            select(h, IpcKind::Hyprland, Some("output"), true).unwrap(),
            Path::Output
        );
        assert_eq!(
            select(h, IpcKind::Hyprland, Some(" "), true).unwrap(),
            Path::Ext
        );
        let s = Avail::from_globals(&sway_112());
        assert!(select(s, IpcKind::Sway, Some("hyprland"), true).is_err());
        assert_eq!(
            select(s, IpcKind::Sway, Some("output"), true).unwrap(),
            Path::Output
        );
        let none = Avail::default();
        assert!(select(none, IpcKind::Sway, Some("ext"), true).is_err());
        assert_eq!(
            select(none, IpcKind::Sway, Some("output"), true).unwrap(),
            Path::Output
        );
        assert!(select(h, IpcKind::Hyprland, Some("none"), true).is_err());
        assert!(select(h, IpcKind::Hyprland, Some("EXT"), true).is_err());
    }

    #[test]
    fn gaps_per_path() {
        let h = Avail::from_globals(&hyprland_056());
        assert_eq!(
            gaps(Path::Ext, h, IpcKind::Hyprland),
            vec!["popups_outside_window"]
        );
        assert_eq!(
            gaps(Path::Hyprland, h, IpcKind::Hyprland),
            vec!["popups_outside_window"]
        );
        let s = Avail::from_globals(&sway_112());
        assert_eq!(
            gaps(Path::Ext, s, IpcKind::Sway),
            vec!["popups_outside_window", "xwayland_unmanaged"]
        );
        assert_eq!(
            gaps(Path::Output, s, IpcKind::Sway),
            ipc::mask_gaps(IpcKind::Sway).to_vec()
        );
        assert_eq!(gaps(Path::Output, h, IpcKind::Hyprland), vec!["popups"]);
        assert_eq!(Path::Ext.protocol(), Some("ext-image-copy-capture-v1"));
        assert_eq!(
            Path::Hyprland.protocol(),
            Some("hyprland-toplevel-export-v1")
        );
        assert_eq!(Path::Output.protocol(), None);
    }

    #[test]
    fn place_scale_one_is_identity() {
        let out = r(0, 0, 1920, 1080);
        let p = place(&r(10, 20, 100, 50), &out, 1920, 1080, 100, 50);
        assert_eq!(
            p,
            Placement {
                x0: 10,
                y0: 20,
                w: 100,
                h: 50,
                resample: false
            }
        );
        // Output not at the origin.
        let out2 = r(1920, 0, 1920, 1080);
        let p = place(&r(1930, 5, 100, 50), &out2, 1920, 1080, 100, 50);
        assert_eq!((p.x0, p.y0), (10, 5));
    }

    #[test]
    fn place_fractional_uses_buffer_size_and_half_up_origin() {
        // 3840x2160 at scale 1.5: logical 2560x1440 (the reporter's setup).
        let out = r(0, 0, 2560, 1440);
        // Odd logical origin and size: 1.5 -> origin 2; width 1277 * 1.5 =
        // 1915.5, the compositor's buffer may be 1915 or 1916.
        for bw in [1915u32, 1916] {
            let p = place(&r(1, 1, 1277, 701), &out, 3840, 2160, bw, 1052);
            assert_eq!(
                p,
                Placement {
                    x0: 2,
                    y0: 2,
                    w: i64::from(bw),
                    h: 1052,
                    resample: false
                }
            );
        }
        // Even origin maps exactly.
        let p = place(&r(10, 40, 1260, 1390), &out, 3840, 2160, 1890, 2085);
        assert_eq!(
            (p.x0, p.y0, p.w, p.h, p.resample),
            (15, 60, 1890, 2085, false)
        );
        // Inward size 1915 - 1 and outward 1916 + 1 are the slack limits.
        assert!(!place(&r(1, 1, 1277, 701), &out, 3840, 2160, 1914, 1052).resample);
        assert!(!place(&r(1, 1, 1277, 701), &out, 3840, 2160, 1917, 1052).resample);
        let p = place(&r(1, 1, 1277, 701), &out, 3840, 2160, 1913, 1052);
        // Outward-rounded rect: floor(1.5) = 1 .. ceil(1917) = 1917.
        assert_eq!(
            p,
            Placement {
                x0: 1,
                y0: 1,
                w: 1916,
                h: 1052,
                resample: true
            }
        );
    }

    #[test]
    fn place_scale_two_and_resize_mismatch() {
        let out = r(0, 0, 960, 540);
        let p = place(&r(5, 7, 10, 10), &out, 1920, 1080, 20, 20);
        assert_eq!((p.x0, p.y0, p.w, p.h, p.resample), (10, 14, 20, 20, false));
        // A window resized mid-capture: buffer half the size -> scaled up
        // into the IPC rect, never past it.
        let p = place(&r(5, 7, 10, 10), &out, 1920, 1080, 10, 10);
        assert_eq!((p.x0, p.y0, p.w, p.h, p.resample), (10, 14, 20, 20, true));
        // Partly left of the output: negative origin, clipped by blit.
        let p = place(&r(-3, 0, 10, 10), &out, 1920, 1080, 20, 20);
        assert_eq!((p.x0, p.w), (-6, 20));
    }

    fn img(w: u32, h: u32, rgba: [u8; 4]) -> WinImage {
        WinImage {
            w,
            h,
            px: rgba
                .iter()
                .copied()
                .cycle()
                .take((w * h * 4) as usize)
                .collect(),
        }
    }

    fn at(c: &RgbImage, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * c.width + x) * 3) as usize;
        [c.pixels[i], c.pixels[i + 1], c.pixels[i + 2]]
    }

    #[test]
    fn blit_exact_clips_and_blends_alpha_over_mask() {
        let mut canvas = RgbImage::mask_canvas(8, 6);
        let opaque = img(3, 2, [200, 100, 50, 255]);
        blit(
            &mut canvas,
            &opaque,
            &Placement {
                x0: 6,
                y0: 5,
                w: 3,
                h: 2,
                resample: false,
            },
        );
        assert_eq!(at(&canvas, 6, 5), [200, 100, 50]);
        assert_eq!(at(&canvas, 7, 5), [200, 100, 50]);
        assert_eq!(at(&canvas, 5, 5), mask::MASK_RGB);
        assert_eq!(at(&canvas, 6, 4), mask::MASK_RGB);
        // Premultiplied half-transparent black over the mask colour.
        let mut c2 = RgbImage::mask_canvas(2, 1);
        blit(
            &mut c2,
            &img(2, 1, [0, 0, 0, 128]),
            &Placement {
                x0: 0,
                y0: 0,
                w: 2,
                h: 1,
                resample: false,
            },
        );
        assert_eq!(at(&c2, 0, 0), [15, 15, 15]);
        // Fully transparent: the mask colour stays.
        let mut c3 = RgbImage::mask_canvas(2, 1);
        blit(
            &mut c3,
            &img(2, 1, [0, 0, 0, 0]),
            &Placement {
                x0: 0,
                y0: 0,
                w: 2,
                h: 1,
                resample: false,
            },
        );
        assert_eq!(at(&c3, 1, 0), mask::MASK_RGB);
    }

    #[test]
    fn blit_resample_fills_the_placement_only() {
        let mut canvas = RgbImage::mask_canvas(10, 10);
        let mut src = img(2, 2, [255, 0, 0, 255]);
        src.px[4..8].copy_from_slice(&[0, 255, 0, 255]); // (1,0) green
        blit(
            &mut canvas,
            &src,
            &Placement {
                x0: 2,
                y0: 2,
                w: 4,
                h: 4,
                resample: true,
            },
        );
        assert_eq!(at(&canvas, 2, 2), [255, 0, 0]);
        assert_eq!(at(&canvas, 5, 2), [0, 255, 0]);
        assert_eq!(at(&canvas, 6, 2), mask::MASK_RGB);
        assert_eq!(at(&canvas, 1, 1), mask::MASK_RGB);
    }

    #[test]
    fn decode_formats_and_y_invert() {
        // ARGB8888 little-endian = B,G,R,A in memory; 2 rows.
        let data = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let f = |format, stride| BufferFormat {
            format,
            width: 1,
            height: 2,
            stride,
        };
        let a = decode_rgba(&data, f(wl_shm::Format::Argb8888, 4), false).unwrap();
        assert_eq!(a.px, vec![30, 20, 10, 40, 70, 60, 50, 80]);
        let x = decode_rgba(&data, f(wl_shm::Format::Xbgr8888, 4), true).unwrap();
        assert_eq!(x.px, vec![50, 60, 70, 255, 10, 20, 30, 255]);
        assert!(decode_rgba(&data, f(wl_shm::Format::Xrgb2101010, 4), false).is_err());
        assert!(decode_rgba(&data[..7], f(wl_shm::Format::Argb8888, 4), false).is_err());
    }

    #[test]
    fn ten_bit_formats_are_refused() {
        let x2r10 = u32::from(wl_shm::Format::Xrgb2101010);
        let a2b10 = u32::from(wl_shm::Format::Abgr2101010);
        assert_eq!(pick_format(&[x2r10, a2b10]), None);
        assert_eq!(pick_format(&[]), None);
        assert_eq!(
            pick_format(&[x2r10, u32::from(wl_shm::Format::Abgr8888)]),
            Some(wl_shm::Format::Abgr8888)
        );
        assert_eq!(
            pick_format(&[0, 1]),
            Some(wl_shm::Format::Xrgb8888),
            "opaque preferred"
        );
    }

    #[test]
    fn transform_rgba_rot90() {
        let src = WinImage {
            w: 2,
            h: 1,
            px: vec![1, 0, 0, 255, 2, 0, 0, 255],
        };
        let t = transform_rgba(src, Transform::Rot90);
        assert_eq!((t.w, t.h), (1, 2));
        assert_eq!(t.px, vec![1, 0, 0, 255, 2, 0, 0, 255]);
    }

    fn snap_with(clients: Vec<Client>) -> Snapshot {
        Snapshot {
            kind: IpcKind::Hyprland,
            clients,
            overlays: Vec::new(),
            corner_radius: 0,
        }
    }

    fn client(app: &str, title: &str, addr: Option<u64>) -> Client {
        Client {
            app_id: app.into(),
            title: title.into(),
            pid: Some(1),
            rect: Some(r(0, 0, 10, 10)),
            content: None,
            output: None,
            workspace: Some("hypr:1".into()),
            visible: true,
            hidden: None,
            layer: ipc::Layer::Tiled,
            xwayland: false,
            address: addr,
            identifier: None,
            stack: 0,
        }
    }

    fn cand(app: &str, title: &str, addr: Option<u64>, ident: Option<&str>) -> Cand {
        Cand {
            closed: false,
            address: addr,
            identifier: ident.map(str::to_owned),
            app_id: app.into(),
            title: title.into(),
        }
    }

    #[test]
    fn source_by_address_when_mapped() {
        // Two identical windows: the address tells them apart.
        let s = snap_with(vec![
            client("foot", "~", Some(7)),
            client("foot", "~", Some(9)),
        ]);
        let cands = [
            cand("foot", "~", Some(9), None),
            cand("foot", "~", Some(7), None),
        ];
        assert_eq!(pick_source(&cands, &s, &s.clients[0], true).unwrap(), 1);
        assert_eq!(pick_source(&cands, &s, &s.clients[1], true).unwrap(), 0);
        // Mapped but the handle has no address (mapping failed): refused,
        // never a fallback to the key.
        let cands = [cand("foot", "~", None, None)];
        let one = snap_with(vec![client("foot", "~", Some(7))]);
        assert!(pick_source(&cands, &one, &one.clients[0], true).is_err());
        // Closed handle is not a candidate.
        let mut c = cand("foot", "~", Some(7), None);
        c.closed = true;
        assert!(pick_source(&[c], &one, &one.clients[0], true).is_err());
    }

    #[test]
    fn source_by_sway_identifier_then_unique_key() {
        let mut s = snap_with(vec![client("foot", "~", None), client("foot", "~", None)]);
        s.kind = IpcKind::Sway;
        s.clients[0].identifier = Some("aa".into());
        s.clients[1].identifier = Some("bb".into());
        let cands = [
            cand("foot", "~", None, Some("bb")),
            cand("foot", "~", None, Some("aa")),
        ];
        assert_eq!(pick_source(&cands, &s, &s.clients[0], false).unwrap(), 1);
        assert_eq!(pick_source(&cands, &s, &s.clients[1], false).unwrap(), 0);
        // Same pair without identifiers: ambiguous, refused.
        let mut s2 = s.clone();
        for c in &mut s2.clients {
            c.identifier = None;
        }
        let plain = [cand("foot", "~", None, None), cand("foot", "~", None, None)];
        assert!(pick_source(&plain, &s2, &s2.clients[0], false).is_err());
        // A unique pair matches by key (wlr handles never carry identifiers).
        let s3 = snap_with(vec![client("foot", "~", None), client("mpv", "v", None)]);
        let c3 = [cand("mpv", "v", None, None), cand("foot", "~", None, None)];
        assert_eq!(pick_source(&c3, &s3, &s3.clients[0], false).unwrap(), 1);
        // Unique among IPC windows but two handles: refused.
        let c4 = [cand("foot", "~", None, None), cand("foot", "~", None, None)];
        assert!(pick_source(&c4, &s3, &s3.clients[0], false).is_err());
        // Identifier known but no handle has it (window gone): refused.
        let c5 = [cand("foot", "~", None, Some("zz"))];
        assert!(pick_source(&c5, &s, &s.clients[0], false).is_err());
    }

    #[test]
    fn address_handle_refuses_low_bit_clashes() {
        let a = 0x0000_0001_dead_beef_u64;
        let b = 0x0000_0002_dead_beef_u64;
        let s = snap_with(vec![client("a", "", Some(a)), client("b", "", Some(0x10))]);
        assert_eq!(address_handle(&s, a).unwrap(), 0xdead_beef);
        let s2 = snap_with(vec![client("a", "", Some(a)), client("b", "", Some(b))]);
        assert!(address_handle(&s2, a).is_err());
    }

    #[test]
    fn rotated_output_never_takes_the_export_path() {
        let h = Avail::from_globals(&hyprland_056());
        assert_eq!(
            select(h, IpcKind::Hyprland, None, false).unwrap(),
            Path::Ext
        );
        assert!(select(h, IpcKind::Hyprland, Some("hyprland"), false).is_err());
        let mut old = hyprland_056();
        old.image_copy_capture = None;
        let o = Avail::from_globals(&old);
        assert_eq!(
            select(o, IpcKind::Hyprland, None, false).unwrap(),
            Path::Output
        );
        assert_eq!(
            select(o, IpcKind::Hyprland, None, true).unwrap(),
            Path::Hyprland
        );
        assert!(select(h, IpcKind::Hyprland, Some("bogus"), true).is_err());
    }

    #[test]
    fn buffer_limits() {
        assert_eq!(check_buffer(100, 50, 400, 7680).unwrap(), 400);
        assert_eq!(
            check_buffer(100, 50, 512, 7680).unwrap(),
            512,
            "row padding"
        );
        assert!(check_buffer(0, 50, 0, 7680).is_err());
        assert!(check_buffer(100, 0, 400, 7680).is_err());
        assert!(check_buffer(7681, 10, 7681 * 4, 7680).is_err(), "oversize");
        assert!(check_buffer(100, 50, 396, 7680).is_err(), "stride below 4w");
        assert!(
            check_buffer(100, 50, 400 + STRIDE_PAD + 4, 7680).is_err(),
            "stride far above 4w"
        );
        // stride * h past i32::MAX.
        assert!(check_buffer(30_000, 30_000, 120_000, MAX_SIDE).is_err());
        assert!(check_buffer(u32::MAX, 1, u32::MAX, u32::MAX).is_err());
        assert_eq!(side_limit(3840, 2160), 15_360);
        assert_eq!(side_limit(10_000, 100), MAX_SIDE);
    }

    #[test]
    fn decode_honours_a_padded_stride() {
        // 1x2 XRGB8888 with 4 bytes of padding per row.
        let data = [1u8, 2, 3, 0, 9, 9, 9, 9, 4, 5, 6, 0, 9, 9, 9, 9];
        let f = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 2,
            stride: 8,
        };
        let img = decode_rgba(&data, f, false).unwrap();
        assert_eq!(img.px, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn transform_rgba_matches_the_output_transform() {
        // Same expectations as capture::output_transform_all_eight on
        //   0 1 2
        //   3 4 5
        let src = || WinImage {
            w: 3,
            h: 2,
            px: (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect(),
        };
        let labels = |w: &WinImage| w.px.chunks(4).map(|p| p[0]).collect::<Vec<u8>>();
        for (t, dims, want) in [
            (Transform::Rot90, (2, 3), [3, 0, 4, 1, 5, 2]),
            (Transform::Rot180, (3, 2), [5, 4, 3, 2, 1, 0]),
            (Transform::Rot270, (2, 3), [2, 5, 1, 4, 0, 3]),
            (Transform::Flipped, (3, 2), [2, 1, 0, 5, 4, 3]),
            (Transform::Flipped90, (2, 3), [0, 3, 1, 4, 2, 5]),
        ] {
            let out = transform_rgba(src(), t);
            assert_eq!((out.w, out.h), dims, "{t:?}");
            assert_eq!(labels(&out), want.to_vec(), "{t:?}");
        }
    }

    #[test]
    fn settle_retries_within_its_cap_and_the_budget() {
        let t0 = Instant::now();
        let far = t0 + Duration::from_secs(10);
        let mut s = Settle::new();
        // Mismatches every ~100 ms: retries until SETTLE_MAX from the first.
        let mut n = 0;
        let mut now = t0;
        while s.next(now, far).is_some() {
            n += 1;
            now += Duration::from_millis(100);
            assert!(n < 100);
        }
        assert_eq!(s.tries, n);
        assert!(now - t0 >= SETTLE_MAX - SETTLE_STEP - Duration::from_millis(100));
        assert!(now - t0 <= SETTLE_MAX + Duration::from_millis(100));
        assert_eq!(n, 8, "800 ms cap, 80 ms pause, 100 ms per attempt");
        // The call's capture deadline ends it earlier.
        let mut s = Settle::new();
        let near = t0 + Duration::from_millis(150);
        assert_eq!(s.next(t0, near), Some(SETTLE_STEP));
        assert_eq!(s.next(t0 + Duration::from_millis(100), near), None);
        assert_eq!(s.tries, 1);
        // A deadline already passed: no retry at all.
        let mut s = Settle::new();
        assert_eq!(s.next(t0, t0), None);
        assert_eq!(s.tries, 0);
    }

    #[test]
    fn handle_list_stability() {
        // (closed, has_address)
        assert!(handles_stable(2, true, &[(false, true), (false, true)]));
        assert!(
            !handles_stable(2, true, &[(false, true), (false, true), (false, true)]),
            "a new handle"
        );
        assert!(
            !handles_stable(2, true, &[(false, true), (false, false)]),
            "no address"
        );
        assert!(
            handles_stable(2, true, &[(false, true), (true, false)]),
            "closed ones do not count"
        );
        assert!(
            handles_stable(2, false, &[(false, false), (false, false)]),
            "by key"
        );
        assert!(!handles_stable(1, false, &[(false, false), (false, false)]));
    }

    #[test]
    fn a_bad_override_never_turns_the_mask_off() {
        let h = Avail::from_globals(&hyprland_056());
        assert_eq!(
            doctor_mask_fields(h, IpcKind::Hyprland, None),
            (
                Some("toplevel"),
                Some("ext-image-copy-capture-v1"),
                vec!["popups_outside_window"]
            )
        );
        assert_eq!(
            doctor_mask_fields(h, IpcKind::Hyprland, Some("output")),
            (Some("output"), None, vec!["popups"])
        );
        let mut no_ext = hyprland_056();
        no_ext.image_copy_capture = None;
        let n = Avail::from_globals(&no_ext);
        for bad in ["Output", "cut", "ext"] {
            let avail = if bad == "ext" { n } else { h };
            let (mode, protocol, gaps) = doctor_mask_fields(avail, IpcKind::Hyprland, Some(bad));
            assert_eq!((mode, protocol), (None, None), "{bad}");
            assert!(!gaps.is_empty(), "{bad}");
            // The masked capture fails closed, naming the override.
            let e = select(avail, IpcKind::Hyprland, Some(bad), true).unwrap_err();
            assert!(e.to_string().contains(OVERRIDE_ENV), "{e}");
        }
    }
}
