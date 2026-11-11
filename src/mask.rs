//! Mask mode for `screenshot` / `zoom`: the image shows the kept windows and
//! nothing else; everything else is [`MASK_RGB`].
//!
//! The plan is computed in global logical coordinates from one IPC snapshot
//! ([`crate::ipc`]) and is read in one of two ways ([`CaptureMode`]):
//!
//! - **Toplevel** (`crate::wincap`): every kept window that the IPC reports
//!   drawn on the captured output is captured on its own and drawn, back to
//!   front, at its IPC rect onto a mask-coloured canvas. A kept window that is
//!   not drawn (other workspace, scratchpad, hidden tab or group member,
//!   another output) is never captured. A kept window that something else
//!   covers is shown whole; `coveredBy` names what may be above it.
//! - **Output** (`crate::capture`): the composited output is captured and all
//!   but the kept windows' visible regions is painted over. A kept window's
//!   visible region is its IPC rect (sway: borders, no title/tab bar;
//!   Hyprland: client box) minus every other visible window's rect unless that
//!   window is known to be below it (a tiled window on the same workspace,
//!   under a floating or fullscreen kept window; floating-over-floating is
//!   always cut), minus Hyprland layer-shell surfaces on the top/overlay
//!   levels, minus the four `corner_radius` squares. The regions are mapped
//!   onto the image (already in logical orientation) rounding kept areas
//!   INWARD, so a pixel only partly covered by a kept window is masked.
//!
//! Window identity: a kept toplevel resolves to its IPC window by the Hyprland
//! window address (`hyprland_toplevel_mapping_v1`) where the compositor has
//! it, else by its app id + title, which must then be unambiguous.
//!
//! Fail closed: unknown window id, a kept window that cannot be matched to its
//! IPC entry without guessing, a visible window with no geometry, or no IPC at
//! all is an error - no image.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::ipc::{self, Client, Layer, Snapshot};
use crate::output::Rect;
use crate::wm::Toplevel;

/// The mask color, #1E1E1E.
pub const MASK_RGB: [u8; 3] = [0x1E, 0x1E, 0x1E];

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskRequest {
    None,
    /// Mask the whole image.
    All,
    /// Keep these bridge window ids (as `windows` prints them), mask the rest.
    Keep(Vec<String>),
}

impl MaskRequest {
    pub fn from_flags(keep: Vec<String>, mask_all: bool) -> Self {
        if mask_all {
            MaskRequest::All
        } else if keep.is_empty() {
            MaskRequest::None
        } else {
            MaskRequest::Keep(keep)
        }
    }
}

/// One kept window as resolved, echoed in the result so the caller can check
/// the id still named the window it listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeptWindow {
    pub id: String,
    pub app_id: String,
    pub title: String,
    /// The window's process id from the same IPC snapshot as the plan (not
    /// from `windows`); null when the compositor gives none or the id does not
    /// resolve to exactly one IPC window.
    pub pid: Option<u32>,
    /// The IPC reports it drawn on some output (kept from before `shown`).
    pub visible: bool,
    /// Its pixels are in the image.
    pub shown: bool,
    /// Why not, when `shown` is false: `other_workspace`, `minimized`,
    /// `other_output`, `covered`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// App ids of windows (`unknown` without one) and `layer:<namespace>` of
    /// top/overlay layer surfaces whose rect meets this window's and that may
    /// be drawn above it.
    pub covered_by: Vec<String>,
}

/// `mask` key of a masked `screenshot` / `zoom` result.
#[derive(Debug, Clone, Serialize)]
pub struct MaskReport {
    /// `"all"` or `"keep"`.
    pub mode: &'static str,
    /// Keep mode: `"toplevel"` (each kept window captured on its own) or
    /// `"output"` (composited output + cut).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<&'static str>,
    pub kept: Vec<KeptWindow>,
    /// Kept areas that are not masked (global logical px, before rounding).
    /// Toplevel mode: the destination rects of the drawn windows.
    pub regions: Vec<Rect>,
    /// Toplevel mode: the override-redirect XWayland windows drawn with a
    /// kept window (menus, dropdowns).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DrawnChild>>,
    /// Toplevel mode: how many such windows of a kept window's process were
    /// left out (shared process, not next to the kept window, no export).
    #[serde(rename = "childrenSkipped", skip_serializing_if = "Option::is_none")]
    pub children_skipped: Option<usize>,
}

/// One drawn override-redirect window in the mask report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DrawnChild {
    /// Hyprland window address, `0x...`.
    pub address: String,
    pub pid: u32,
    /// The kept window id it is drawn with.
    pub parent: String,
    pub rect: Rect,
}

/// The keep plan from one snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Output mode: kept rects after the cut, global logical px (two kept
    /// windows' rects may overlap). Toplevel mode: the drawn windows' rects
    /// clipped to the output.
    pub keep: Vec<Rect>,
    pub kept: Vec<KeptWindow>,
    /// Toplevel mode: the kept windows to capture and draw, back to front.
    pub draws: Vec<Client>,
    /// Toplevel mode on Hyprland: XWayland windows that are not toplevels
    /// (override-redirect menus, dropdowns, tooltips) of a drawn XWayland
    /// client, matched by pid, back to front.
    pub children: Vec<Client>,
    /// Per child, the kept window id it belongs to.
    pub child_parents: Vec<String>,
    /// Candidates left out (pid shared with a window that is not kept, or
    /// not next to a drawn kept window of that pid).
    pub children_skipped: usize,
}

/// A half-open pixel rect `[x0, x1) x [y0, y1)` in image coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

// ------------------------------------------------------------- rect math

fn right(r: &Rect) -> i64 {
    i64::from(r.x) + i64::from(r.width)
}
fn bottom(r: &Rect) -> i64 {
    i64::from(r.y) + i64::from(r.height)
}

fn mk(x0: i64, y0: i64, x1: i64, y1: i64) -> Option<Rect> {
    (x1 > x0 && y1 > y0).then(|| Rect {
        x: x0 as i32,
        y: y0 as i32,
        width: (x1 - x0) as i32,
        height: (y1 - y0) as i32,
    })
}

pub fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    mk(
        i64::from(a.x.max(b.x)),
        i64::from(a.y.max(b.y)),
        right(a).min(right(b)),
        bottom(a).min(bottom(b)),
    )
}

/// `a` minus `cut`, as up to four disjoint rects (top, bottom, left, right bands).
pub fn subtract_one(a: &Rect, cut: &Rect) -> Vec<Rect> {
    let Some(i) = intersect(a, cut) else {
        return vec![*a];
    };
    let (ax0, ay0, ax1, ay1) = (i64::from(a.x), i64::from(a.y), right(a), bottom(a));
    let (ix0, iy0, ix1, iy1) = (i64::from(i.x), i64::from(i.y), right(&i), bottom(&i));
    [
        mk(ax0, ay0, ax1, iy0),
        mk(ax0, iy1, ax1, ay1),
        mk(ax0, iy0, ix0, iy1),
        mk(ix1, iy0, ax1, iy1),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn subtract(region: Vec<Rect>, cut: &Rect) -> Vec<Rect> {
    region.iter().flat_map(|r| subtract_one(r, cut)).collect()
}

/// The four `radius x radius` corner squares of `r` (clamped to its size).
fn corners(r: &Rect, radius: i32) -> Vec<Rect> {
    if radius <= 0 {
        return Vec::new();
    }
    let w = radius.min(r.width);
    let h = radius.min(r.height);
    let (x1, y1) = (r.x + r.width - w, r.y + r.height - h);
    [(r.x, r.y), (x1, r.y), (r.x, y1), (x1, y1)]
        .into_iter()
        .map(|(x, y)| Rect {
            x,
            y,
            width: w,
            height: h,
        })
        .collect()
}

fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}
fn ceil_div(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}

/// Map kept logical rects to the pixels of an image of `img_w x img_h` that
/// shows the logical area `output`, rounding inward (exact integer math, so a
/// 1.5 scale or a rotated output cannot leak a partly covered pixel).
pub fn to_pixels(keep: &[Rect], output: &Rect, img_w: u32, img_h: u32) -> Vec<PixRect> {
    if output.width <= 0 || output.height <= 0 {
        return Vec::new();
    }
    let (lw, lh) = (i64::from(output.width), i64::from(output.height));
    let (iw, ih) = (i64::from(img_w), i64::from(img_h));
    keep.iter()
        .filter_map(|r| {
            let r = intersect(r, output)?;
            let lx0 = i64::from(r.x - output.x);
            let ly0 = i64::from(r.y - output.y);
            let x0 = ceil_div(lx0 * iw, lw).clamp(0, iw);
            let y0 = ceil_div(ly0 * ih, lh).clamp(0, ih);
            let x1 = floor_div((lx0 + i64::from(r.width)) * iw, lw).clamp(0, iw);
            let y1 = floor_div((ly0 + i64::from(r.height)) * ih, lh).clamp(0, ih);
            (x1 > x0 && y1 > y0).then_some(PixRect {
                x0: x0 as u32,
                y0: y0 as u32,
                x1: x1 as u32,
                y1: y1 as u32,
            })
        })
        .collect()
}

// ------------------------------------------------------------------- plan

/// Is `n` known to be drawn below `k` (so it cannot cover it)?
fn known_below(n: &Client, k: &Client) -> bool {
    n.workspace.is_some()
        && n.workspace == k.workspace
        && n.layer == Layer::Tiled
        && k.layer != Layer::Tiled
}

fn describe(c: &Client) -> String {
    format!(
        "{} \"{}\"",
        if c.app_id.is_empty() { "?" } else { &c.app_id },
        c.title
    )
}

fn meets(a: &Rect, b: &Rect) -> bool {
    intersect(a, b).is_some()
}

/// How the image is made, and so how the plan is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// The composited output is captured and everything but the kept
    /// windows' visible regions is painted over.
    Output,
    /// Each kept window is captured on its own and drawn onto a mask canvas.
    Toplevel,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureMode::Output => "output",
            CaptureMode::Toplevel => "toplevel",
        }
    }
}

/// What the plan needs besides the windows.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    /// Logical rect of the captured output.
    pub output: &'a Rect,
    pub mode: CaptureMode,
    /// Kept toplevels are located by their Hyprland window address
    /// (`Toplevel::address`, from `hyprland_toplevel_mapping_v1`) instead of
    /// their app id + title.
    pub by_address: bool,
    /// Toplevel mode: override-redirect children may be drawn (false when the
    /// toplevel handle list may be stale, see `wincap::Session::handles_stable`).
    pub children_allowed: bool,
}

/// Whether one kept IPC window is in the image, and why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Shown,
    NotShown(&'static str),
}

/// Who may cover `k` (index `ki`): app ids of other visible windows that are
/// not kept and not known to be below it, and `layer:<namespace>` of top /
/// overlay layer surfaces, each when its rect meets `k`'s. `conservative`
/// treats every such window as possibly above (the output cut's rule);
/// otherwise a window lower in the stack on the same workspace is skipped.
fn covered_by(snap: &Snapshot, ki: usize, skip: &[usize], conservative: bool) -> Vec<String> {
    let k = &snap.clients[ki];
    let Some(kr) = k.rect else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !names.contains(&s) {
            names.push(s);
        }
    };
    for (ni, n) in snap.clients.iter().enumerate() {
        if ni == ki || skip.contains(&ni) || !n.visible || known_below(n, k) {
            continue;
        }
        let Some(nr) = n.rect else { continue };
        if !meets(&nr, &kr) {
            continue;
        }
        if !conservative && n.workspace.is_some() && n.workspace == k.workspace && n.stack < k.stack
        {
            continue;
        }
        push(if n.app_id.is_empty() {
            "unknown".to_owned()
        } else {
            n.app_id.clone()
        });
    }
    for o in &snap.overlays {
        if meets(&o.rect, &kr) {
            let ns = if o.namespace.is_empty() {
                "unknown"
            } else {
                &o.namespace
            };
            push(format!("layer:{ns}"));
        }
    }
    names
}

/// Resolve the kept ids against the toplevels and the snapshot and work out,
/// per kept window, whether and where it is in the image. See the module doc.
pub fn plan(
    toplevels: &[Toplevel],
    keep_ids: &[String],
    snap: &Snapshot,
    ctx: PlanContext<'_>,
) -> Result<Plan> {
    let mut keep_idx: Vec<usize> = Vec::new();
    for id in keep_ids {
        let Some(i) = toplevels.iter().position(|t| &t.id == id) else {
            bail!("window `{id}` not found; call windows for current ids");
        };
        if !keep_idx.contains(&i) {
            keep_idx.push(i);
        }
    }
    fn tkey(t: &Toplevel) -> (&str, &str) {
        ipc::key(t.app_id.as_deref(), t.title.as_deref())
    }

    // Kept client indices, and per kept toplevel the clients it resolved to.
    let mut kept_clients: Vec<usize> = Vec::new();
    let mut per_toplevel: Vec<Vec<usize>> = Vec::new();
    for &ti in &keep_idx {
        let t = &toplevels[ti];
        let k = tkey(t);
        let clients: Vec<usize> = if ctx.by_address {
            let Some(addr) = t.address else {
                bail!(
                    "window `{}` has no compositor window address; cannot locate it",
                    t.id
                );
            };
            let hits: Vec<usize> = snap
                .clients
                .iter()
                .enumerate()
                .filter(|(_, c)| c.address == Some(addr))
                .map(|(i, _)| i)
                .collect();
            if hits.len() != 1 {
                bail!(
                    "window `{}` (address 0x{addr:x}) has {} compositor match(es); cannot locate it",
                    t.id,
                    hits.len()
                );
            }
            // An address can be reused by a new window once the old one is
            // gone: the app must still be the one the handle names.
            let c = &snap.clients[hits[0]];
            if c.app_id != k.0 {
                bail!(
                    "window `{}` is `{}` but its compositor window (address 0x{addr:x}) is `{}`; masked capture refused",
                    t.id,
                    k.0,
                    c.app_id
                );
            }
            hits
        } else {
            if k.0.is_empty() && k.1.is_empty() {
                bail!(
                    "window `{}` has no app id and no title; cannot locate it",
                    t.id
                );
            }
            let group: Vec<&Toplevel> = toplevels.iter().filter(|o| tkey(o) == k).collect();
            if let Some(other) = group
                .iter()
                .find(|o| !keep_idx.iter().any(|&i| toplevels[i].id == o.id))
            {
                bail!(
                    "window `{}` cannot be told apart from window `{}` (same app id and title); keep both or neither",
                    t.id,
                    other.id
                );
            }
            let clients: Vec<usize> = snap
                .clients
                .iter()
                .enumerate()
                .filter(|(_, c)| c.app_id == k.0 && c.title == k.1)
                .map(|(i, _)| i)
                .collect();
            if clients.len() != group.len() {
                bail!(
                    "window `{}` ({} \"{}\") has {} compositor match(es) for {} window(s); cannot locate it",
                    t.id,
                    k.0,
                    k.1,
                    clients.len(),
                    group.len()
                );
            }
            clients
        };
        for c in &clients {
            if !kept_clients.contains(c) {
                kept_clients.push(*c);
            }
        }
        per_toplevel.push(clients);
    }

    // Every visible window must have geometry, kept or not: an occluder with no
    // rect could cover anything.
    if let Some(c) = snap.clients.iter().find(|c| c.visible && c.rect.is_none()) {
        bail!(
            "visible window {} reports no geometry; masked capture refused",
            describe(c)
        );
    }

    // A kept client counts as minimized when any toplevel kept for it says so.
    let minimized = |ci: usize| {
        keep_idx
            .iter()
            .zip(&per_toplevel)
            .any(|(&ti, cs)| cs.contains(&ci) && toplevels[ti].minimized)
    };

    let mut keep = Vec::new();
    let mut draws: Vec<(usize, Client)> = Vec::new();
    let mut status: Vec<(usize, Status)> = Vec::new();
    for &ki in &kept_clients {
        let k = &snap.clients[ki];
        let st = match (k.visible, k.rect) {
            (false, _) | (true, None) => {
                Status::NotShown(k.hidden.unwrap_or(ipc::Hidden::OtherWorkspace).reason())
            }
            (true, Some(rect)) => match ctx.mode {
                CaptureMode::Toplevel if minimized(ki) => Status::NotShown("minimized"),
                CaptureMode::Toplevel => {
                    match k.draw_rect().and_then(|d| intersect(&d, ctx.output)) {
                        None => Status::NotShown("other_output"),
                        Some(dest) => {
                            keep.push(dest);
                            draws.push((ki, k.clone()));
                            Status::Shown
                        }
                    }
                }
                CaptureMode::Output => {
                    let mut region = vec![rect];
                    for (ni, n) in snap.clients.iter().enumerate() {
                        if ni == ki || kept_clients.contains(&ni) || !n.visible || known_below(n, k)
                        {
                            continue;
                        }
                        if let Some(nr) = n.rect {
                            region = subtract(region, &nr);
                        }
                    }
                    for o in &snap.overlays {
                        region = subtract(region, &o.rect);
                    }
                    for c in corners(&rect, snap.corner_radius) {
                        region = subtract(region, &c);
                    }
                    let on_output = region.iter().any(|r| meets(r, ctx.output));
                    keep.extend(region);
                    if on_output {
                        Status::Shown
                    } else if meets(&rect, ctx.output) {
                        Status::NotShown("covered")
                    } else {
                        Status::NotShown("other_output")
                    }
                }
            },
        };
        status.push((ki, st));
    }
    draws.sort_by_key(|(_, c)| c.stack);
    let kept_id_of = |ci: usize| {
        keep_idx
            .iter()
            .zip(&per_toplevel)
            .find(|(_, cs)| cs.contains(&ci))
            .map(|(&ti, _)| toplevels[ti].id.clone())
            .unwrap_or_default()
    };

    // Override-redirect windows (menus, dropdowns) of a drawn XWayland client:
    // Hyprland lists them as clients that are not toplevels, with the pid of
    // the X client connection that created them (XRes). A process can own
    // windows of several apps (LibreOffice, a browser and its web apps,
    // Wine), so a child is drawn only when no window of that pid is left out
    // and it lies on or next to a drawn kept window of that pid.
    let mut children: Vec<Client> = Vec::new();
    let mut child_parents: Vec<String> = Vec::new();
    let mut child_idx: Vec<usize> = Vec::new();
    let mut children_skipped = 0usize;
    if ctx.mode == CaptureMode::Toplevel && snap.kind == crate::ax::IpcKind::Hyprland {
        let parents: Vec<(u32, Rect, String)> = draws
            .iter()
            .filter(|(_, c)| c.xwayland)
            .filter_map(|(ci, c)| Some((c.pid?, c.draw_rect()?, kept_id_of(*ci))))
            .collect();
        let is_toplevel = |n: &Client| {
            if ctx.by_address {
                toplevels
                    .iter()
                    .any(|t| t.address.is_some() && t.address == n.address)
            } else {
                toplevels
                    .iter()
                    .any(|t| tkey(t) == (n.app_id.as_str(), n.title.as_str()))
            }
        };
        // A drawable child: a visible XWayland window without a class (X11
        // menus, dropdowns and tooltips carry no WM_CLASS; Hyprland's IPC has
        // no override-redirect flag) that no toplevel handle names.
        let child_like =
            |n: &Client| n.xwayland && n.visible && n.app_id.is_empty() && !is_toplevel(n);
        for (ni, n) in snap.clients.iter().enumerate() {
            let Some(pid) = n.pid else { continue };
            if kept_clients.contains(&ni)
                || !child_like(n)
                || !parents.iter().any(|(p, ..)| *p == pid)
            {
                continue;
            }
            let (Some(rect), Some(_)) = (n.rect, n.address) else {
                children_skipped += 1;
                continue;
            };
            if !meets(&rect, ctx.output) {
                continue;
            }
            // (a) every other window of this process is kept or a child.
            let shared = snap.clients.iter().enumerate().any(|(mi, m)| {
                mi != ni && m.pid == Some(pid) && !kept_clients.contains(&mi) && !child_like(m)
            });
            // (b) on or touching a drawn kept window of the same pid.
            let parent = parents.iter().find(|(p, r, _)| {
                *p == pid
                    && meets(
                        &rect,
                        &Rect {
                            x: r.x.saturating_sub(1),
                            y: r.y.saturating_sub(1),
                            width: r.width.saturating_add(2),
                            height: r.height.saturating_add(2),
                        },
                    )
            });
            match (shared, parent) {
                (false, Some((_, _, id))) if ctx.children_allowed => {
                    children.push(n.clone());
                    child_parents.push(id.clone());
                    child_idx.push(ni);
                }
                _ => children_skipped += 1,
            }
        }
        let mut order: Vec<usize> = (0..children.len()).collect();
        order.sort_by_key(|&i| children[i].stack);
        children = order.iter().map(|&i| children[i].clone()).collect();
        child_parents = order.iter().map(|&i| child_parents[i].clone()).collect();
        keep.extend(
            children
                .iter()
                .filter_map(|c| c.draw_rect().and_then(|r| intersect(&r, ctx.output))),
        );
    }
    let draws: Vec<Client> = draws.into_iter().map(|(_, c)| c).collect();

    // What may cover each kept window (never a kept window or a drawn child).
    let skip: Vec<usize> = kept_clients.iter().chain(&child_idx).copied().collect();
    let status: Vec<(usize, Status, Vec<String>)> = status
        .into_iter()
        .map(|(ki, st)| {
            let conservative = ctx.mode == CaptureMode::Output || snap.clients[ki].hidden.is_some();
            let cov = match st {
                Status::Shown | Status::NotShown("covered") => {
                    covered_by(snap, ki, &skip, conservative)
                }
                Status::NotShown(_) => Vec::new(),
            };
            (ki, st, cov)
        })
        .collect();

    let mut kept = Vec::new();
    for (&ti, cs) in keep_idx.iter().zip(&per_toplevel) {
        let t = &toplevels[ti];
        // Report what the compositor says now, not the handle's cached state.
        let c0 = &snap.clients[cs[0]];
        let mine: Vec<&(usize, Status, Vec<String>)> =
            status.iter().filter(|(ci, ..)| cs.contains(ci)).collect();
        let shown = mine.iter().any(|(_, s, _)| *s == Status::Shown);
        let reason = if shown {
            None
        } else {
            mine.iter().find_map(|(_, s, _)| match s {
                Status::NotShown(r) => Some(*r),
                Status::Shown => None,
            })
        };
        let mut covered: Vec<String> = Vec::new();
        for (_, _, cov) in &mine {
            for c in cov {
                if !covered.contains(c) {
                    covered.push(c.clone());
                }
            }
        }
        kept.push(KeptWindow {
            id: t.id.clone(),
            app_id: c0.app_id.clone(),
            title: c0.title.clone(),
            pid: match cs.as_slice() {
                [only] => snap.clients[*only].pid,
                _ => None,
            },
            visible: cs.iter().any(|&c| snap.clients[c].visible),
            shown,
            reason,
            covered_by: covered,
        });
    }
    Ok(Plan {
        keep,
        kept,
        draws,
        children,
        child_parents,
        children_skipped,
    })
}

/// What of a window decides the image: identity, app, geometry, visibility,
/// workspace and output. Not the title, not the focus-recency number (the
/// draw ORDER is compared separately).
type DrawKey = (
    Option<u64>,
    Option<String>,
    String,
    Option<Rect>,
    Option<Rect>,
    bool,
    Option<ipc::Hidden>,
    Option<String>,
    Option<String>,
);

fn draw_key(c: &Client) -> DrawKey {
    (
        c.address,
        c.identifier.clone(),
        c.app_id.clone(),
        c.rect,
        c.content,
        c.visible,
        c.hidden,
        c.workspace.clone(),
        c.output.clone(),
    )
}

impl Plan {
    /// Whether two plans would produce the same image: same kept regions,
    /// same kept-window verdicts, and the same windows drawn in the same
    /// order. Titles, focus recency and `coveredBy` do not change the image.
    pub fn same_image(&self, other: &Plan, _mode: CaptureMode) -> bool {
        let kept = |p: &Plan| {
            p.kept
                .iter()
                .map(|k| {
                    (
                        k.id.clone(),
                        k.app_id.clone(),
                        k.pid,
                        k.visible,
                        k.shown,
                        k.reason,
                    )
                })
                .collect::<Vec<_>>()
        };
        let keys = |v: &[Client]| v.iter().map(draw_key).collect::<Vec<_>>();
        self.keep == other.keep
            && kept(self) == kept(other)
            && keys(&self.draws) == keys(&other.draws)
            && keys(&self.children) == keys(&other.children)
            && self.child_parents == other.child_parents
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::tests::{hypr_fixture, r, sway_fixture};
    use crate::ipc::{parse_hypr, parse_sway};

    fn tl(id: &str, app: &str, title: &str) -> Toplevel {
        Toplevel::for_test(id, app, title)
    }

    /// The whole logical plane: no kept window is on another output.
    fn everywhere() -> Rect {
        r(-100_000, -100_000, 200_000, 200_000)
    }

    fn plan(toplevels: &[Toplevel], ids: &[String], snap: &Snapshot) -> Result<Plan> {
        super::plan(
            toplevels,
            ids,
            snap,
            PlanContext {
                output: &everywhere(),
                mode: CaptureMode::Output,
                by_address: false,
                children_allowed: true,
            },
        )
    }

    fn area(v: &[Rect]) -> i64 {
        v.iter()
            .map(|r| i64::from(r.width) * i64::from(r.height))
            .sum()
    }

    fn covers(v: &[Rect], x: i32, y: i32) -> bool {
        v.iter()
            .any(|r| x >= r.x && y >= r.y && x < r.x + r.width && y < r.y + r.height)
    }

    #[test]
    fn subtract_bands_are_disjoint_and_exact() {
        let a = r(0, 0, 10, 10);
        let parts = subtract_one(&a, &r(3, 4, 2, 2));
        assert_eq!(area(&parts), 96);
        for (i, p) in parts.iter().enumerate() {
            for q in &parts[i + 1..] {
                assert!(intersect(p, q).is_none());
            }
        }
        assert_eq!(subtract_one(&a, &r(20, 20, 5, 5)), vec![a]);
        assert!(subtract_one(&a, &r(-5, -5, 30, 30)).is_empty());
    }

    #[test]
    fn pixels_scale_one_is_identity() {
        let out = r(0, 0, 1280, 800);
        let px = to_pixels(&[r(10, 20, 100, 50)], &out, 1280, 800);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 10,
                y0: 20,
                x1: 110,
                y1: 70
            }]
        );
    }

    #[test]
    fn pixels_fractional_scale_round_inward() {
        // Output at 1280,0, 1280x720 logical, 1920x1080 pixels (scale 1.5).
        let out = r(1280, 0, 1280, 720);
        // Logical x 1281..1284 -> physical 1.5..6.0: pixel 1 is half covered -> masked.
        let px = to_pixels(&[r(1281, 1, 3, 3)], &out, 1920, 1080);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 2,
                y0: 2,
                x1: 6,
                y1: 6
            }]
        );
        // Even coords map exactly.
        let px = to_pixels(&[r(1282, 2, 2, 2)], &out, 1920, 1080);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 3,
                y0: 3,
                x1: 6,
                y1: 6
            }]
        );
        // A 1-px logical keep at an odd offset keeps 1 whole pixel, never 2.
        let px = to_pixels(&[r(1281, 1, 1, 1)], &out, 1920, 1080);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 2,
                y0: 2,
                x1: 3,
                y1: 3
            }]
        );
    }

    #[test]
    fn pixels_rotated_output_uses_logical_orientation() {
        // 1280x720 panel, transform 90: logical 720x1280 at 0,800, image 720x1280.
        let out = r(0, 800, 720, 1280);
        let px = to_pixels(&[r(100, 900, 50, 60)], &out, 720, 1280);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 100,
                y0: 100,
                x1: 150,
                y1: 160
            }]
        );
        // Keeps on other outputs and off-image parts are dropped / clipped.
        assert!(to_pixels(&[r(800, 0, 10, 10)], &out, 720, 1280).is_empty());
        let px = to_pixels(&[r(-10, 790, 20, 20)], &out, 720, 1280);
        assert_eq!(
            px,
            vec![PixRect {
                x0: 0,
                y0: 0,
                x1: 10,
                y1: 10
            }]
        );
    }

    #[test]
    fn pixels_odd_ratio_never_overreach() {
        // 1.25 scale with a 1707-wide odd logical width: every kept pixel must
        // lie fully inside the logical rect.
        let out = r(0, 0, 1707, 960);
        let (iw, ih) = (2134u32, 1200u32);
        for x in 0..40 {
            for w in 1..9 {
                let k = r(x, 3, w, 5);
                for p in to_pixels(&[k], &out, iw, ih) {
                    // pixel [p.x0, p.x1) in logical units: p * lw / iw
                    let lx0 = f64::from(p.x0) * 1707.0 / f64::from(iw);
                    let lx1 = f64::from(p.x1) * 1707.0 / f64::from(iw);
                    assert!(lx0 >= f64::from(k.x) - 1e-9, "{k:?} {p:?}");
                    assert!(lx1 <= f64::from(k.x + k.width) + 1e-9, "{k:?} {p:?}");
                }
            }
        }
    }

    fn sway_toplevels() -> Vec<Toplevel> {
        vec![
            tl("1", "org.gnome.TextEditor", "Editor"),
            tl("2", "firefox", "Docs"),
            tl("3", "firefox", "Mail"),
            tl("4", "org.gnome.Calculator", "Calculator"),
            tl("5", "XTerm", "xterm"),
            tl("6", "foot", "Hidden"),
            tl("7", "mpv", "Video"),
            tl("8", "foot", "Term"),
            tl("9", "scratchy", "scratch"),
        ]
    }

    #[test]
    fn sway_tiled_keep_minus_floating_occluders() {
        let snap = parse_sway(&sway_fixture());
        let p = plan(&sway_toplevels(), &["1".into()], &snap).unwrap();
        assert_eq!(p.kept.len(), 1);
        assert!(p.kept[0].visible);
        // Editor 0,0 640x800 minus Calculator (500,300 400x300) minus xterm (100,100 200x100).
        assert!(covers(&p.keep, 0, 0));
        assert!(
            !covers(&p.keep, 550, 350),
            "floating calculator over it is masked"
        );
        assert!(
            !covers(&p.keep, 150, 150),
            "floating xterm over it is masked"
        );
        assert!(
            !covers(&p.keep, 700, 100),
            "the tiled neighbour is not kept"
        );
        assert_eq!(
            area(&p.keep),
            640 * 800 - 140 * 300 - 200 * 100,
            "only the overlaps are cut"
        );
    }

    #[test]
    fn sway_floating_keep_is_not_cut_by_tiled_below() {
        let snap = parse_sway(&sway_fixture());
        let p = plan(&sway_toplevels(), &["4".into()], &snap).unwrap();
        // Calculator is floating; tiled Editor/Docs are below it, xterm does not overlap.
        assert_eq!(p.keep, vec![r(500, 300, 400, 300)]);
    }

    #[test]
    fn sway_floating_vs_floating_is_conservative() {
        let snap = parse_sway(&sway_fixture());
        // Move xterm over the calculator: either could be on top -> cut.
        let mut snap2 = snap.clone();
        let x = snap2
            .clients
            .iter_mut()
            .find(|c| c.title == "xterm")
            .unwrap();
        x.rect = Some(r(450, 250, 100, 100));
        let p = plan(&sway_toplevels(), &["4".into()], &snap2).unwrap();
        assert!(!covers(&p.keep, 520, 320));
        assert!(covers(&p.keep, 600, 400));
    }

    #[test]
    fn hidden_kept_window_keeps_nothing() {
        let snap = parse_sway(&sway_fixture());
        let p = plan(&sway_toplevels(), &["3".into(), "6".into()], &snap).unwrap();
        assert!(p.keep.is_empty());
        assert!(p.kept.iter().all(|k| !k.visible));
    }

    #[test]
    fn unknown_id_and_ambiguity_fail_closed() {
        let snap = parse_sway(&sway_fixture());
        let e = plan(&sway_toplevels(), &["99".into()], &snap).unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");

        // Two toplevels with the same key: keeping one is refused, both is fine.
        let mut tls = sway_toplevels();
        tls.push(tl("10", "foot", "Term"));
        let mut snap2 = snap.clone();
        let mut t2 = snap2
            .clients
            .iter()
            .find(|c| c.title == "Term")
            .unwrap()
            .clone();
        t2.rect = Some(r(0, 800, 360, 1280));
        snap2.clients.push(t2);
        let e = plan(&tls, &["8".into()], &snap2).unwrap_err();
        assert!(e.to_string().contains("cannot be told apart"), "{e}");
        let p = plan(&tls, &["8".into(), "10".into()], &snap2).unwrap();
        assert_eq!(p.kept.len(), 2);
        assert!(covers(&p.keep, 0, 800) && covers(&p.keep, 719, 2079));

        // Toplevel present, IPC match missing (title changed in between).
        let mut snap3 = snap.clone();
        snap3
            .clients
            .iter_mut()
            .find(|c| c.title == "Editor")
            .unwrap()
            .title = "Editor *".into();
        let e = plan(&sway_toplevels(), &["1".into()], &snap3).unwrap_err();
        assert!(e.to_string().contains("cannot locate"), "{e}");
    }

    #[test]
    fn visible_window_without_geometry_fails_closed() {
        let mut snap = parse_sway(&sway_fixture());
        snap.clients
            .iter_mut()
            .find(|c| c.title == "Video")
            .unwrap()
            .rect = None;
        let e = plan(&sway_toplevels(), &["1".into()], &snap).unwrap_err();
        assert!(e.to_string().contains("no geometry"), "{e}");
    }

    fn hypr_toplevels() -> Vec<Toplevel> {
        vec![
            tl("1", "kitty", "~"),
            tl("2", "firefox", "Docs"),
            tl("3", "firefox", "Mail"),
            tl("4", "steam", "Friends"),
            tl("5", "pavucontrol", "Volume"),
            tl("6", "mpv", "Video"),
            tl("7", "code", "Project"),
        ]
    }

    #[test]
    fn hypr_keep_minus_special_floating_layers_and_corners() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(8));
        let p = plan(&hypr_toplevels(), &["1".into()], &snap).unwrap();
        // kitty 10,40 840x910; cut by: waybar (y<30: no overlap), steam floating
        // (300,200 400x300), special-workspace pavucontrol (100,100 600x400),
        // and 8px corners.
        assert!(covers(&p.keep, 20, 600));
        assert!(
            !covers(&p.keep, 150, 150),
            "special workspace window is above"
        );
        assert!(!covers(&p.keep, 650, 450), "floating steam is above");
        assert!(!covers(&p.keep, 10, 40), "rounded corner masked");
        assert!(!covers(&p.keep, 849, 949), "rounded corner masked");
        assert!(covers(&p.keep, 18, 940));
        // Inactive-workspace window is not an occluder.
        assert!(covers(&p.keep, 800, 900));
    }

    #[test]
    fn hypr_notification_overlay_cuts_keep() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let p = plan(&hypr_toplevels(), &["2".into()], &snap).unwrap();
        // firefox Docs 860,40 836x910; notification 1300,40 380x90 on overlay.
        assert!(!covers(&p.keep, 1400, 100));
        assert!(covers(&p.keep, 1400, 200));
        assert!(covers(&p.keep, 900, 45));
    }

    #[test]
    fn hypr_fullscreen_keep_and_hidden_group_member() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let p = plan(&hypr_toplevels(), &["6".into()], &snap).unwrap();
        assert_eq!(p.keep, vec![r(1706, 0, 1080, 1920)]);
        let p = plan(&hypr_toplevels(), &["3".into()], &snap).unwrap();
        assert!(p.keep.is_empty());
        assert!(!p.kept[0].visible);
    }

    fn ctx(output: &Rect, mode: CaptureMode, by_address: bool) -> PlanContext<'_> {
        PlanContext {
            output,
            mode,
            by_address,
            children_allowed: true,
        }
    }

    fn kept<'a>(p: &'a Plan, id: &str) -> &'a KeptWindow {
        p.kept.iter().find(|k| k.id == id).unwrap()
    }

    #[test]
    fn toplevel_mode_visibility_gate_and_reasons_sway() {
        let snap = parse_sway(&sway_fixture());
        let out = r(0, 0, 1280, 800); // HEADLESS-1
        let ids: Vec<String> = ["1", "3", "6", "7", "9"].map(String::from).to_vec();
        let p = super::plan(
            &sway_toplevels(),
            &ids,
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        // Only the drawn window on the requested output is captured.
        assert_eq!(p.draws.len(), 1);
        assert_eq!(p.draws[0].title, "Editor");
        assert_eq!(
            p.keep,
            vec![r(2, 2, 636, 796)],
            "the client area (sway window_rect), no cut"
        );
        let ed = kept(&p, "1");
        assert!(ed.shown && ed.reason.is_none());
        // Covered by the floating calculator and xterm: shown whole, named.
        assert_eq!(ed.covered_by, vec!["org.gnome.Calculator", "XTerm"]);
        let mail = kept(&p, "3");
        assert!(!mail.shown && !mail.visible);
        assert_eq!(mail.reason, Some("covered"), "hidden tab");
        assert_eq!(
            mail.covered_by,
            vec!["firefox", "org.gnome.Calculator"],
            "the visible tab and the floating window over it"
        );
        assert_eq!(kept(&p, "6").reason, Some("other_workspace"));
        assert!(kept(&p, "6").covered_by.is_empty());
        let video = kept(&p, "7");
        assert!(video.visible && !video.shown);
        assert_eq!(video.reason, Some("other_output"));
        assert_eq!(kept(&p, "9").reason, Some("minimized"), "scratchpad");
        assert!(
            p.children.is_empty(),
            "sway: no override-redirect windows in IPC"
        );
    }

    #[test]
    fn toplevel_mode_minimized_toplevel_is_never_drawn() {
        let snap = parse_sway(&sway_fixture());
        let mut tls = sway_toplevels();
        tls[0].minimized = true;
        let out = r(0, 0, 1280, 800);
        let p = super::plan(
            &tls,
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.draws.is_empty() && p.keep.is_empty());
        assert_eq!(p.kept[0].reason, Some("minimized"));
    }

    /// Hyprland at 3840x2160 scale 1.5 (logical 2560x1440) with Omarchy's
    /// notification host: a transparent full-output overlay layer.
    fn hypr_omarchy_popup() -> Snapshot {
        let (c, m, mut l) = hypr_fixture();
        l["DP-1"]["levels"]["3"] = serde_json::json!([
            {"address":"0xd","x":0,"y":0,"w":1706,"h":960,"namespace":"omarchy-notifications","pid":9}
        ]);
        parse_hypr(&c, &m, Some(&l), Some(8))
    }

    #[test]
    fn full_output_overlay_output_mode_reports_covered() {
        let snap = hypr_omarchy_popup();
        let out = r(0, 0, 1706, 960);
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Output, false),
        )
        .unwrap();
        assert!(p.keep.is_empty(), "the cut leaves nothing");
        let k = &p.kept[0];
        assert!(k.visible && !k.shown);
        assert_eq!(k.reason, Some("covered"));
        assert!(
            k.covered_by
                .contains(&"layer:omarchy-notifications".to_owned())
        );
        assert!(k.covered_by.contains(&"steam".to_owned()));
        assert!(p.draws.is_empty());
    }

    #[test]
    fn full_output_overlay_toplevel_mode_shows_the_window() {
        let snap = hypr_omarchy_popup();
        let out = r(0, 0, 1706, 960);
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert_eq!(p.keep, vec![r(10, 40, 840, 910)]);
        assert_eq!(p.draws.len(), 1);
        let k = &p.kept[0];
        assert!(k.shown && k.reason.is_none());
        assert_eq!(
            k.covered_by,
            vec!["steam", "pavucontrol", "layer:omarchy-notifications"]
        );
        // Not named: firefox Docs (tiled neighbour, no overlap), code
        // (inactive workspace), mpv (other monitor), waybar (above it).
    }

    #[test]
    fn toplevel_mode_floating_kept_over_tiled_is_not_covered() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let out = r(0, 0, 1706, 960);
        // steam (floating, focused last) over kitty (tiled): kitty is known below.
        let p = super::plan(
            &hypr_toplevels(),
            &["4".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.kept[0].shown);
        assert_eq!(
            p.kept[0].covered_by,
            vec!["pavucontrol"],
            "special workspace is above"
        );
        // Two kept windows: drawn back to front, neither names the other.
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into(), "4".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        let order: Vec<&str> = p.draws.iter().map(|c| c.app_id.as_str()).collect();
        assert_eq!(order, vec!["kitty", "steam"]);
        assert!(!kept(&p, "1").covered_by.contains(&"steam".to_owned()));
    }

    #[test]
    fn toplevel_mode_fullscreen_and_other_output() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let hdmi = r(1706, 0, 1080, 1920);
        let p = super::plan(
            &hypr_toplevels(),
            &["6".into(), "1".into()],
            &snap,
            ctx(&hdmi, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert_eq!(p.keep, vec![r(1706, 0, 1080, 1920)]);
        assert!(kept(&p, "6").shown);
        assert_eq!(kept(&p, "1").reason, Some("other_output"));
        // A closed special workspace / group member: never drawn.
        let p = super::plan(
            &hypr_toplevels(),
            &["3".into(), "7".into()],
            &snap,
            ctx(&hdmi, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.draws.is_empty());
        assert_eq!(kept(&p, "3").reason, Some("covered"));
        assert_eq!(kept(&p, "7").reason, Some("other_workspace"));
    }

    fn with_addresses(mut tls: Vec<Toplevel>) -> Vec<Toplevel> {
        for (i, t) in tls.iter_mut().enumerate() {
            t.address = Some(i as u64 + 1);
        }
        tls
    }

    #[test]
    fn address_identity_tells_twins_apart() {
        let (c, m, l) = hypr_fixture();
        let mut snap = parse_hypr(&c, &m, Some(&l), Some(0));
        // A second "kitty ~" (address 0x9) on the same workspace.
        let mut twin = snap.clients[0].clone();
        twin.address = Some(9);
        twin.rect = Some(r(860, 500, 400, 300));
        snap.clients.push(twin);
        let mut tls = with_addresses(hypr_toplevels());
        let mut t2 = Toplevel::for_test("8", "kitty", "~");
        t2.address = Some(9);
        tls.push(t2);
        let out = r(0, 0, 1706, 960);
        // By key: refused.
        let e = super::plan(
            &tls,
            &["8".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot be told apart"), "{e}");
        // By address: exactly the twin, in both modes.
        let p = super::plan(
            &tls,
            &["8".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, true),
        )
        .unwrap();
        assert!(p.kept[0].shown);
        assert_eq!(p.keep, vec![r(860, 500, 400, 300)]);
        assert_eq!(p.draws[0].address, Some(9));
        // A tiled window lower in the stack is not named in toplevel mode.
        assert!(p.kept[0].covered_by.is_empty());
        let p = super::plan(
            &tls,
            &["8".into()],
            &snap,
            ctx(&out, CaptureMode::Output, true),
        )
        .unwrap();
        assert!(!covers(&p.keep, 20, 600), "the other kitty is not kept");
        // The output cut is conservative: the tiled firefox may be above.
        assert_eq!(p.kept[0].reason, Some("covered"));
        assert_eq!(p.kept[0].covered_by, vec!["firefox"]);
        // No address on the kept toplevel, or none / two IPC windows with it: refused.
        let mut none = tls.clone();
        none[7].address = None;
        assert!(
            super::plan(
                &none,
                &["8".into()],
                &snap,
                ctx(&out, CaptureMode::Toplevel, true)
            )
            .is_err()
        );
        let mut gone = tls.clone();
        gone[7].address = Some(77);
        let e = super::plan(
            &gone,
            &["8".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, true),
        )
        .unwrap_err();
        assert!(e.to_string().contains("cannot locate"), "{e}");
        let mut dup = snap.clone();
        let mut again = dup.clients[8].clone();
        again.rect = Some(r(0, 0, 5, 5));
        dup.clients.push(again);
        assert!(
            super::plan(
                &tls,
                &["8".into()],
                &dup,
                ctx(&out, CaptureMode::Toplevel, true)
            )
            .is_err()
        );
    }

    fn or_child(pid: u32, rect: Rect, addr: u64) -> Client {
        Client {
            app_id: String::new(),
            title: String::new(),
            pid: Some(pid),
            rect: Some(rect),
            output: Some("DP-1".into()),
            workspace: Some("hypr:1".into()),
            visible: true,
            hidden: None,
            layer: Layer::Floating,
            xwayland: true,
            content: None,
            address: Some(addr),
            identifier: None,
            stack: 2_000_000,
        }
    }

    #[test]
    fn xwayland_override_redirect_children_follow_a_drawn_parent() {
        let (c, m, l) = hypr_fixture();
        let mut snap = parse_hypr(&c, &m, Some(&l), Some(0));
        // A dropdown of steam (XWayland, pid 33) and one of another X client.
        snap.clients.push(or_child(33, r(320, 260, 200, 150), 0x20));
        snap.clients
            .push(or_child(999, r(330, 270, 100, 100), 0x21));
        let out = r(0, 0, 1706, 960);
        for by_address in [false, true] {
            let tls = if by_address {
                let mut t = with_addresses(hypr_toplevels());
                // fixture addresses are 0x1..0x8 in client order; steam is 0x4.
                for (tl, a) in t.iter_mut().zip([1u64, 2, 3, 4, 5, 6, 7]) {
                    tl.address = Some(a);
                }
                t
            } else {
                hypr_toplevels()
            };
            let p = super::plan(
                &tls,
                &["4".into()],
                &snap,
                ctx(&out, CaptureMode::Toplevel, by_address),
            )
            .unwrap();
            assert_eq!(p.children.len(), 1, "by_address {by_address}");
            assert_eq!(p.children[0].address, Some(0x20));
            assert!(p.keep.contains(&r(320, 260, 200, 150)));
            // The drawn child is not a coverer; the foreign one is.
            assert_eq!(p.kept[0].covered_by, vec!["pavucontrol", "unknown"]);
            // Output mode never draws children.
            let p = super::plan(
                &tls,
                &["4".into()],
                &snap,
                ctx(&out, CaptureMode::Output, by_address),
            )
            .unwrap();
            assert!(p.children.is_empty());
        }
        // A non-XWayland kept window gets no children; nor a parent not drawn.
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.children.is_empty());
        let hdmi = r(1706, 0, 1080, 1920);
        let p = super::plan(
            &hypr_toplevels(),
            &["4".into()],
            &snap,
            ctx(&hdmi, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.children.is_empty());
    }

    #[test]
    fn same_image_ignores_covered_by_only_in_toplevel_mode() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let popup = hypr_omarchy_popup();
        let out = r(0, 0, 1706, 960);
        let t = |s: &Snapshot, mode| {
            super::plan(&hypr_toplevels(), &["1".into()], s, ctx(&out, mode, false)).unwrap()
        };
        let (a, b) = (
            t(&snap, CaptureMode::Toplevel),
            t(&popup, CaptureMode::Toplevel),
        );
        assert_ne!(a.kept, b.kept, "coveredBy differs");
        assert!(
            a.same_image(&b, CaptureMode::Toplevel),
            "a popup does not change the window"
        );
        let (a, b) = (
            t(&snap, CaptureMode::Output),
            t(&popup, CaptureMode::Output),
        );
        assert!(!a.same_image(&b, CaptureMode::Output));
        // A moved window is a different image in toplevel mode.
        let mut moved = snap.clone();
        moved.clients[0].rect = Some(r(12, 40, 840, 910));
        let (a, b) = (
            t(&snap, CaptureMode::Toplevel),
            t(&moved, CaptureMode::Toplevel),
        );
        assert!(!a.same_image(&b, CaptureMode::Toplevel));
        // So is a window that changed workspace in between.
        let mut away = snap.clone();
        away.clients[0].visible = false;
        away.clients[0].hidden = Some(ipc::Hidden::OtherWorkspace);
        let b = t(&away, CaptureMode::Toplevel);
        assert!(!a.same_image(&b, CaptureMode::Toplevel));
        assert!(b.draws.is_empty());
    }

    #[test]
    fn report_serializes_contract_fields() {
        let snap = hypr_omarchy_popup();
        let out = r(0, 0, 1706, 960);
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into(), "7".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        let rep = MaskReport {
            mode: "keep",
            capture: Some(CaptureMode::Toplevel.as_str()),
            kept: p.kept,
            regions: p.keep,
            children: Some(Vec::new()),
            children_skipped: Some(0),
        };
        let v = serde_json::to_value(&rep).unwrap();
        assert_eq!(v["capture"], "toplevel");
        assert_eq!(v["kept"][0]["shown"], true);
        assert!(v["kept"][0].get("reason").is_none());
        assert_eq!(v["kept"][0]["coveredBy"][2], "layer:omarchy-notifications");
        assert_eq!(v["kept"][1]["shown"], false);
        assert_eq!(v["kept"][1]["visible"], false);
        assert_eq!(v["kept"][1]["reason"], "other_workspace");
        assert_eq!(v["kept"][1]["coveredBy"], serde_json::json!([]));
        assert_eq!(v["kept"][0]["appId"], "kitty");
        assert_eq!(v["kept"][0]["pid"], 11, "from the snapshot");
        assert_eq!(v["kept"][1]["pid"], 66);
        assert_eq!(v["regions"][0]["width"], 840);
        // Mask-all keeps its old shape.
        let all = MaskReport {
            mode: "all",
            capture: None,
            kept: Vec::new(),
            regions: Vec::new(),
            children: None,
            children_skipped: None,
        };
        assert_eq!(
            serde_json::to_value(&all).unwrap(),
            serde_json::json!({"mode":"all","kept":[],"regions":[]})
        );
    }

    #[test]
    fn toplevel_mode_still_fails_closed() {
        let snap = parse_sway(&sway_fixture());
        let out = r(0, 0, 1280, 800);
        let c = ctx(&out, CaptureMode::Toplevel, false);
        assert!(super::plan(&sway_toplevels(), &["99".into()], &snap, c).is_err());
        let mut tls = sway_toplevels();
        tls.push(tl("10", "foot", "Term"));
        let mut snap2 = snap.clone();
        let t2 = snap2
            .clients
            .iter()
            .find(|c| c.title == "Term")
            .unwrap()
            .clone();
        snap2.clients.push(t2);
        let e = super::plan(&tls, &["8".into()], &snap2, c).unwrap_err();
        assert!(e.to_string().contains("cannot be told apart"), "{e}");
        let mut snap3 = snap.clone();
        snap3
            .clients
            .iter_mut()
            .find(|c| c.title == "Video")
            .unwrap()
            .rect = None;
        assert!(super::plan(&sway_toplevels(), &["1".into()], &snap3, c).is_err());
    }

    /// Hyprland fixture plus a steam dropdown (pid 33) next to steam.
    fn hypr_with_dropdown() -> Snapshot {
        let (c, m, l) = hypr_fixture();
        let mut snap = parse_hypr(&c, &m, Some(&l), Some(0));
        snap.clients.push(or_child(33, r(320, 260, 200, 150), 0x20));
        snap
    }

    fn other_app_same_pid(visible: bool) -> Client {
        let mut w = or_child(33, r(900, 100, 300, 300), 0x30);
        w.app_id = "steamwebhelper".into();
        w.title = "Store".into();
        w.layer = Layer::Tiled;
        w.visible = visible;
        w.hidden = (!visible).then_some(ipc::Hidden::OtherWorkspace);
        w.workspace = Some(if visible { "hypr:1" } else { "hypr:3" }.into());
        w
    }

    #[test]
    fn child_with_a_pid_shared_by_a_window_not_kept_is_skipped() {
        let out = r(0, 0, 1706, 960);
        for visible in [true, false] {
            let mut snap = hypr_with_dropdown();
            snap.clients.push(other_app_same_pid(visible));
            let mut tls = hypr_toplevels();
            tls.push(tl("8", "steamwebhelper", "Store"));
            let p = super::plan(
                &tls,
                &["4".into()],
                &snap,
                ctx(&out, CaptureMode::Toplevel, false),
            )
            .unwrap();
            assert!(p.children.is_empty(), "visible {visible}");
            assert_eq!(p.children_skipped, 1);
            assert!(!p.keep.contains(&r(320, 260, 200, 150)));
            // Kept as well: the process is fully kept, the child is drawn.
            let p = super::plan(
                &tls,
                &["4".into(), "8".into()],
                &snap,
                ctx(&out, CaptureMode::Toplevel, false),
            )
            .unwrap();
            assert_eq!(p.children.len(), 1, "visible {visible}");
            assert_eq!(p.child_parents, vec!["4"]);
        }
    }

    #[test]
    fn child_far_from_its_parent_is_skipped_touching_is_drawn() {
        let out = r(0, 0, 1706, 960);
        let (c, m, l) = hypr_fixture();
        let mut snap = parse_hypr(&c, &m, Some(&l), Some(0));
        // steam is 300,200 400x300: one menu touching its right edge, one far.
        snap.clients.push(or_child(33, r(700, 250, 100, 100), 0x21));
        snap.clients
            .push(or_child(33, r(1200, 800, 100, 100), 0x22));
        let p = super::plan(
            &hypr_toplevels(),
            &["4".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        let addrs: Vec<Option<u64>> = p.children.iter().map(|c| c.address).collect();
        assert_eq!(addrs, vec![Some(0x21)]);
        assert_eq!(p.children_skipped, 1);
    }

    #[test]
    fn address_identity_refuses_an_app_mismatch() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let out = r(0, 0, 1706, 960);
        // Handle 1 says kitty at address 0x1; the compositor's 0x1 is now foot
        // (kitty closed, its address reused).
        let mut tls = with_addresses(hypr_toplevels());
        tls[0].address = Some(1);
        let mut snap2 = snap.clone();
        snap2.clients[0].app_id = "foot".into();
        for mode in [CaptureMode::Toplevel, CaptureMode::Output] {
            let e = super::plan(&tls, &["1".into()], &snap2, ctx(&out, mode, true)).unwrap_err();
            assert!(e.to_string().contains("refused"), "{e}");
            assert!(super::plan(&tls, &["1".into()], &snap, ctx(&out, mode, true)).is_ok());
        }
        // kept[] reports the compositor's current title, not the handle's.
        let mut snap3 = snap.clone();
        snap3.clients[0].title = "~/src".into();
        let p = super::plan(
            &tls,
            &["1".into()],
            &snap3,
            ctx(&out, CaptureMode::Toplevel, true),
        )
        .unwrap();
        assert_eq!(
            (p.kept[0].app_id.as_str(), p.kept[0].title.as_str()),
            ("kitty", "~/src")
        );
    }

    #[test]
    fn same_image_ignores_titles_and_focus_but_not_geometry_or_order() {
        let (c, m, l) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(&l), Some(0));
        let out = r(0, 0, 1706, 960);
        let tls = with_addresses(hypr_toplevels());
        let ids: Vec<String> = vec!["1".into(), "4".into()];
        let t = |s: &Snapshot| {
            super::plan(&tls, &ids, s, ctx(&out, CaptureMode::Toplevel, true)).unwrap()
        };
        let base = t(&snap);
        let same = |s: &Snapshot| base.same_image(&t(s), CaptureMode::Toplevel);
        // Title only.
        let mut x = snap.clone();
        x.clients[0].title = "vim".into();
        assert!(same(&x), "title change");
        // Focus moved, kitty still below steam.
        let mut x = snap.clone();
        x.clients[0].stack += 7;
        assert!(same(&x), "focus recency without reorder");
        // Rect, workspace, app id (address reused), order: different.
        let mut x = snap.clone();
        x.clients[0].rect = Some(r(10, 41, 840, 910));
        assert!(!same(&x), "rect");
        let mut x = snap.clone();
        x.clients[3].workspace = Some("hypr:5".into());
        assert!(!same(&x), "workspace");
        let mut x = snap.clone();
        x.clients[3].stack = 0; // steam now below kitty
        assert!(!same(&x), "draw order");
        let mut x = snap.clone();
        x.clients[3].output = Some("HDMI-A-1".into());
        assert!(!same(&x), "output");
        // The app id change is refused outright by the address check.
        let mut x = snap.clone();
        x.clients[3].app_id = "other".into();
        assert!(super::plan(&tls, &ids, &x, ctx(&out, CaptureMode::Toplevel, true)).is_err());
    }

    #[test]
    fn hyprland_tiled_kept_under_a_fullscreen_window_is_shown_whole() {
        let (c, m, l) = hypr_fixture();
        let mut snap = parse_hypr(&c, &m, Some(&l), Some(0));
        // mpv fullscreen on kitty's workspace and monitor.
        let mpv = snap.clients.iter_mut().find(|c| c.app_id == "mpv").unwrap();
        mpv.workspace = Some("hypr:1".into());
        mpv.output = Some("DP-1".into());
        mpv.rect = Some(r(0, 0, 1706, 960));
        let out = r(0, 0, 1706, 960);
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert_eq!(p.draws.len(), 1);
        assert_eq!(p.keep, vec![r(10, 40, 840, 910)]);
        assert!(p.kept[0].shown);
        assert!(p.kept[0].covered_by.contains(&"mpv".to_owned()));
        // The output cut finds nothing of it.
        let p = super::plan(
            &hypr_toplevels(),
            &["1".into()],
            &snap,
            ctx(&out, CaptureMode::Output, false),
        )
        .unwrap();
        assert_eq!(p.kept[0].reason, Some("covered"));
    }

    #[test]
    fn drawn_child_serializes() {
        let c = DrawnChild {
            address: "0x20".into(),
            pid: 33,
            parent: "4".into(),
            rect: r(1, 2, 3, 4),
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            serde_json::json!({"address":"0x20","pid":33,"parent":"4","rect":{"x":1,"y":2,"width":3,"height":4}})
        );
        let rep = MaskReport {
            mode: "keep",
            capture: Some("toplevel"),
            kept: Vec::new(),
            regions: Vec::new(),
            children: Some(vec![c]),
            children_skipped: Some(2),
        };
        let v = serde_json::to_value(&rep).unwrap();
        assert_eq!(v["childrenSkipped"], 2);
        assert_eq!(v["children"][0]["parent"], "4");
    }

    #[test]
    fn stale_handle_list_draws_no_children() {
        let out = r(0, 0, 1706, 960);
        let snap = hypr_with_dropdown();
        let c = PlanContext {
            output: &out,
            mode: CaptureMode::Toplevel,
            by_address: false,
            children_allowed: false,
        };
        let p = super::plan(&hypr_toplevels(), &["4".into()], &snap, c).unwrap();
        assert!(p.children.is_empty());
        assert_eq!(p.children_skipped, 1);
        assert!(!p.keep.contains(&r(320, 260, 200, 150)));
    }

    #[test]
    fn a_new_toplevel_of_the_same_pid_is_never_a_child() {
        // steam (pid 33) is kept; the same process opens a window that is
        // not in the handle list yet (class set, touching steam): neither it
        // nor steam's own dropdown is drawn.
        let out = r(0, 0, 1706, 960);
        let mut snap = hypr_with_dropdown();
        let mut calc = or_child(33, r(700, 200, 300, 300), 0x40);
        calc.app_id = "libreoffice-calc".into();
        calc.title = "Untitled 1".into();
        snap.clients.push(calc);
        let p = super::plan(
            &hypr_toplevels(),
            &["4".into()],
            &snap,
            ctx(&out, CaptureMode::Toplevel, false),
        )
        .unwrap();
        assert!(p.children.is_empty());
        assert_eq!(
            p.children_skipped, 1,
            "the dropdown; the new window is no candidate"
        );
        assert!(!p.keep.contains(&r(700, 200, 300, 300)));
    }
}
