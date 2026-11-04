//! Mask mode for `screenshot` / `zoom`: everything except the visible parts of
//! the kept windows is painted [`MASK_RGB`].
//!
//! The plan is computed in global logical coordinates from one IPC snapshot
//! ([`crate::ipc`]); `capture` maps it onto the captured output image, which is
//! already in logical orientation (the output transform is undone by
//! `capture::apply_output_transform`), so the mapping is a per-axis scale.
//! Kept areas are rounded INWARD to whole pixels, i.e. masked areas outward: a
//! pixel only partly covered by a kept window is masked.
//!
//! What a kept window's visible region is:
//! - its IPC rect (sway: borders, no title/tab bar; Hyprland: client box),
//! - minus every other visible window's rect unless that window is known to be
//!   below it (a tiled window on the same workspace, under a floating or
//!   fullscreen kept window). Floating-over-floating is always cut: conservative,
//!   the non-kept one may be on top,
//! - minus Hyprland layer-shell surfaces on the top/overlay levels,
//! - minus the four `corner_radius` squares (rounded corners show what is below).
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
    /// Drawn at capture time (false: nothing of it is in the image).
    pub visible: bool,
}

/// `mask` key of a masked `screenshot` / `zoom` result.
#[derive(Debug, Clone, Serialize)]
pub struct MaskReport {
    /// `"all"` or `"keep"`.
    pub mode: &'static str,
    pub kept: Vec<KeptWindow>,
    /// Kept areas that are not masked (global logical px, before rounding).
    pub regions: Vec<Rect>,
}

/// The keep plan from one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Kept rects, global logical px (two kept windows' rects may overlap).
    pub keep: Vec<Rect>,
    pub kept: Vec<KeptWindow>,
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

/// Resolve the kept ids against the toplevels and the snapshot and compute the
/// kept regions. See the module doc for the rules.
pub fn plan(toplevels: &[Toplevel], keep_ids: &[String], snap: &Snapshot) -> Result<Plan> {
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

    // Kept client indices, and per kept toplevel the client it resolved to.
    let mut kept_clients: Vec<usize> = Vec::new();
    let mut kept = Vec::new();
    for &ti in &keep_idx {
        let t = &toplevels[ti];
        let k = tkey(t);
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
        for c in &clients {
            if !kept_clients.contains(c) {
                kept_clients.push(*c);
            }
        }
        kept.push(KeptWindow {
            id: t.id.clone(),
            app_id: k.0.to_owned(),
            title: k.1.to_owned(),
            visible: clients.iter().any(|&c| snap.clients[c].visible),
        });
    }

    // Every visible window must have geometry, kept or not: an occluder with no
    // rect could cover anything.
    if let Some(c) = snap.clients.iter().find(|c| c.visible && c.rect.is_none()) {
        bail!(
            "visible window {} reports no geometry; masked capture refused",
            describe(c)
        );
    }

    let mut keep = Vec::new();
    for &ki in &kept_clients {
        let k = &snap.clients[ki];
        let (true, Some(rect)) = (k.visible, k.rect) else {
            continue;
        };
        let mut region = vec![rect];
        for (ni, n) in snap.clients.iter().enumerate() {
            if ni == ki || kept_clients.contains(&ni) || !n.visible || known_below(n, k) {
                continue;
            }
            if let Some(nr) = n.rect {
                region = subtract(region, &nr);
            }
        }
        for o in &snap.overlays {
            region = subtract(region, o);
        }
        for c in corners(&rect, snap.corner_radius) {
            region = subtract(region, &c);
        }
        keep.extend(region);
    }
    Ok(Plan { keep, kept })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::tests::{hypr_fixture, r, sway_fixture};
    use crate::ipc::{parse_hypr, parse_sway};

    fn tl(id: &str, app: &str, title: &str) -> Toplevel {
        Toplevel::for_test(id, app, title)
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
}
