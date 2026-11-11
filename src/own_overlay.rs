//! Masked captures look through the caller's own Computer Use overlay.
//!
//! `wlroots-bridge overlay` (cu-overlay) draws Claude's glow and drawn cursor
//! on a full-output, click-through layer surface on the overlay level, with
//! the layer-shell namespace [`OVERLAY_NAMESPACE`]. Hyprland's mask
//! ([`crate::mask`]) cuts every level 2/3 layer surface out of the kept
//! windows, so without this rule that surface would leave no kept pixel on
//! its output: every masked screenshot would be solid #1E1E1E while the
//! overlay is up.
//!
//! [`is_own_overlay`] decides which layer surface is "ours, look through it".
//! ALL of these must hold:
//!
//! 1. its namespace is exactly [`OVERLAY_NAMESPACE`];
//! 2. this bridge has a caller: `getppid()` > 1. Claude Desktop runs every
//!    bridge command directly from its main process (`execFile` /
//!    `spawn`), so our parent is the app;
//! 3. the compositor reports the surface's pid (the Wayland client's peer
//!    credentials), and that pid is the caller itself or a process whose
//!    parent is the caller: the app's own `wlroots-bridge overlay` child, a
//!    sibling of this bridge process. A grandchild (a shell an agent
//!    started, a process an MCP server forked) does not qualify.
//!
//! Anything else stays an ordinary occluder (fail closed: the area under it
//! is masked, as it was before this rule).
//!
//! Safety: looking through a surface only ever removes ONE cut. A kept
//! region is still the kept window's IPC rect minus every other window,
//! every other layer surface and the rounded corners, so the rule can never
//! keep anything outside the kept windows and can never reveal a window that
//! is not kept. Over a kept window the image shows what the compositor
//! composited there: the window plus the overlay's own rim / arrow pixels
//! (the same as x11-bridge's `--ignore-window`). A wrong positive decision
//! can only let pixels of a direct child of the app through over a kept
//! window; a wrong negative decision masks the kept window under the overlay
//! (the pre-rule behavior).
//!
//! sway's IPC does not list layer surfaces at all (`screenshot_mask_gaps`:
//! `layer_shell_top_overlay`), so on sway the overlay never cut kept windows
//! and this rule is not consulted.

/// Layer-shell namespace of the `overlay` subcommand's surfaces.
pub const OVERLAY_NAMESPACE: &str = cu_overlay::backend::layer_shell::NAMESPACE;

/// Is the layer surface with this `namespace` and compositor-reported `pid`
/// the caller's own overlay, to be looked through by the mask? Uses this
/// process's parent pid and `/proc/<pid>/stat`. See the module doc.
pub fn is_own_overlay(namespace: Option<&str>, pid: Option<u32>) -> bool {
    if namespace != Some(OVERLAY_NAMESPACE) {
        // No /proc read for the common case (every foreign surface).
        return false;
    }
    // SAFETY: getppid has no memory effects.
    let caller = u32::try_from(unsafe { libc::getppid() }).ok();
    is_own_overlay_with(namespace, pid, caller, proc_ppid)
}

/// [`is_own_overlay`] with the caller pid and the parent-pid lookup given.
pub fn is_own_overlay_with(
    namespace: Option<&str>,
    pid: Option<u32>,
    caller: Option<u32>,
    ppid_of: impl Fn(u32) -> Option<u32>,
) -> bool {
    if namespace != Some(OVERLAY_NAMESPACE) {
        return false;
    }
    let (Some(caller), Some(pid)) = (caller.filter(|&p| p > 1), pid.filter(|&p| p > 0)) else {
        return false;
    };
    pid == caller || ppid_of(pid) == Some(caller)
}

/// Parent pid of a running process (`None` when unreadable, zombie or dead).
fn proc_ppid(pid: u32) -> Option<u32> {
    parse_stat_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Field 4 of `/proc/<pid>/stat`, counted after the last `)` (the command
/// name may contain spaces and parentheses).
fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    if matches!(f.next()?, "Z" | "X" | "x") {
        return None;
    }
    f.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::parse_hypr;
    use crate::ipc::tests::{hypr_fixture, r};
    use crate::mask::{CaptureMode, Plan, PlanContext, plan};
    use crate::output::Rect;
    use crate::wm::Toplevel;
    use serde_json::{Value, json};

    const NS: Option<&str> = Some(OVERLAY_NAMESPACE);

    /// Caller (the app) = 500; 900 its child (our sibling); 901 a grandchild;
    /// 950 an unrelated process; 960 a child of another process.
    fn ppid(pid: u32) -> Option<u32> {
        match pid {
            900 => Some(500),
            901 => Some(900),
            950 => Some(1),
            960 => Some(600),
            _ => None,
        }
    }

    #[test]
    fn namespace_is_cu_overlays() {
        assert_eq!(OVERLAY_NAMESPACE, "claude-cu-overlay");
    }

    #[test]
    fn right_namespace_and_own_process_tree() {
        assert!(
            is_own_overlay_with(NS, Some(900), Some(500), ppid),
            "sibling"
        );
        assert!(
            is_own_overlay_with(NS, Some(500), Some(500), ppid),
            "caller"
        );
    }

    #[test]
    fn wrong_namespace_is_never_ours() {
        for ns in [
            None,
            Some(""),
            Some("waybar"),
            Some("claude-cu-overlay2"),
            Some("Claude-cu-overlay"),
            Some(" claude-cu-overlay"),
        ] {
            assert!(
                !is_own_overlay_with(ns, Some(900), Some(500), ppid),
                "{ns:?}"
            );
            assert!(!is_own_overlay(ns, Some(std::process::id())), "{ns:?}");
        }
    }

    #[test]
    fn right_namespace_foreign_pid_is_not_ours() {
        for pid in [950, 960, 901, 4242] {
            assert!(
                !is_own_overlay_with(NS, Some(pid), Some(500), ppid),
                "{pid}"
            );
        }
        assert!(!is_own_overlay_with(NS, None, Some(500), ppid), "no pid");
        assert!(!is_own_overlay_with(NS, Some(0), Some(500), ppid), "pid 0");
        // No caller: orphaned (parent 1) or unknown.
        assert!(!is_own_overlay_with(NS, Some(950), Some(1), ppid));
        assert!(!is_own_overlay_with(NS, Some(900), None, ppid));
    }

    #[test]
    fn stat_ppid_parsing() {
        let stat =
            "900 (wlroots-bridge) S 500 900 500 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 1234 0";
        assert_eq!(parse_stat_ppid(stat), Some(500));
        assert_eq!(parse_stat_ppid("7 (a) b) R 3 7 7"), Some(3));
        assert_eq!(parse_stat_ppid("7 (dead) Z 3 7 7"), None);
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[test]
    fn real_process_tree() {
        // This test process is a child of its runner, so for a pretend bridge
        // whose parent is that runner, this process is a sibling.
        // SAFETY: getppid has no memory effects.
        let runner = unsafe { libc::getppid() } as u32;
        assert_eq!(proc_ppid(std::process::id()), Some(runner));
        assert!(is_own_overlay(NS, Some(std::process::id())));
        // pid 1 (init) is never a child of our runner.
        assert!(!is_own_overlay(NS, Some(1)));
    }

    // ------------------------------------------ through parse_hypr + plan

    /// `j/layers` as Hyprland prints it (address, x, y, w, h, namespace, pid
    /// per surface), plus a full-output overlay-level surface on DP-1.
    fn layers_with(namespace: &str, pid: Value) -> Value {
        json!({
          "DP-1":{"levels":{
            "0":[{"address":"0x55d0a1","x":0,"y":0,"w":1706,"h":960,"namespace":"hyprpaper","pid":1201}],
            "1":[],
            "2":[{"address":"0x55d0b2","x":0,"y":0,"w":1706,"h":30,"namespace":"waybar","pid":1202}],
            "3":[{"address":"0x55d0c3","x":1300,"y":40,"w":380,"h":90,"namespace":"notifications","pid":1203},
                 {"address":"0x55d0d4","x":0,"y":0,"w":1706,"h":960,"namespace":namespace,"pid":pid}]}},
          "HDMI-A-1":{"levels":{"0":[],"1":[],"2":[],"3":[]}}
        })
    }

    fn toplevels() -> Vec<Toplevel> {
        [
            ("1", "kitty", "~"),
            ("2", "firefox", "Docs"),
            ("3", "firefox", "Mail"),
            ("4", "steam", "Friends"),
            ("5", "pavucontrol", "Volume"),
            ("6", "mpv", "Video"),
            ("7", "code", "Project"),
        ]
        .iter()
        .map(|(i, a, t)| Toplevel::for_test(i, a, t))
        .collect()
    }

    /// The output-cut plan (the path where layer surfaces matter) for one
    /// kept window, with `layers` as `j/layers`.
    fn plan_with(layers: &Value, keep: &str) -> Plan {
        plan_mode(layers, keep, CaptureMode::Output)
    }

    fn plan_mode(layers: &Value, keep: &str, mode: CaptureMode) -> Plan {
        let (c, m, _) = hypr_fixture();
        let snap = parse_hypr(&c, &m, Some(layers), Some(8));
        let everywhere = r(-10_000, -10_000, 20_000, 20_000);
        let ctx = PlanContext {
            output: &everywhere,
            mode,
            by_address: false,
            children_allowed: true,
        };
        plan(&toplevels(), &[keep.into()], &snap, ctx).unwrap()
    }

    fn keep_with(layers: &Value, keep: &str) -> Vec<Rect> {
        plan_with(layers, keep).keep
    }

    fn keep_without_overlay(keep: &str) -> Vec<Rect> {
        let (_, _, l) = hypr_fixture();
        keep_with(&l, keep)
    }

    const OUR_LAYER: &str = "layer:claude-cu-overlay";

    fn covers(v: &[Rect], x: i32, y: i32) -> bool {
        v.iter()
            .any(|r| x >= r.x && y >= r.y && x < r.x + r.width && y < r.y + r.height)
    }

    #[test]
    fn hypr_own_overlay_is_looked_through_exactly() {
        // This test process stands in for the overlay (a sibling, see
        // real_process_tree).
        let me = json!(std::process::id());
        let layers = layers_with(OVERLAY_NAMESPACE, me);
        for keep in ["1", "2", "6"] {
            // Exactly the plan with no overlay surface: never wider, never narrower.
            assert_eq!(
                keep_with(&layers, keep),
                keep_without_overlay(keep),
                "{keep}"
            );
            let with = plan_with(&layers, keep);
            let (_, _, l) = hypr_fixture();
            let without = plan_with(&l, keep);
            assert_eq!(with.kept, without.kept, "{keep}: same report");
            assert!(with.kept[0].shown, "{keep}: kept window shown");
            assert!(
                !with.kept[0].covered_by.iter().any(|c| c == OUR_LAYER),
                "{keep}: our overlay is not in coveredBy"
            );
        }
        // Per-window capture (the Hyprland default): no layer is in the image
        // anyway; the report does not name our overlay either.
        let t = plan_mode(&layers, "1", CaptureMode::Toplevel);
        assert!(t.kept[0].shown);
        assert!(!t.kept[0].covered_by.iter().any(|c| c == OUR_LAYER));
        let k = keep_with(&layers, "2");
        assert!(!covers(&k, 1400, 100), "notification still cuts");
        assert!(covers(&k, 1400, 200));
        let k = keep_with(&layers, "1");
        assert!(!covers(&k, 150, 150), "special workspace window still cuts");
        assert!(!covers(&k, 10, 40), "rounded corner still cuts");
        let kitty = r(10, 40, 840, 910);
        assert!(
            k.iter()
                .all(|p| crate::mask::intersect(p, &kitty) == Some(*p)),
            "never outside the kept window"
        );
    }

    #[test]
    fn hypr_foreign_or_misnamed_overlay_still_masks() {
        let me = json!(std::process::id());
        // Wrong namespace, our own process tree: an ordinary occluder.
        let p = plan_with(&layers_with("waybar-2", me), "1");
        assert!(p.keep.is_empty());
        assert!(!p.kept[0].shown);
        assert!(p.kept[0].covered_by.iter().any(|c| c == "layer:waybar-2"));
        // Right namespace, not our process tree / no pid.
        for pid in [json!(1), json!(0), json!(-5), Value::Null] {
            let p = plan_with(&layers_with(OVERLAY_NAMESPACE, pid.clone()), "1");
            assert!(p.keep.is_empty(), "{pid}");
            assert!(!p.kept[0].shown, "{pid}");
            assert!(
                p.kept[0].covered_by.iter().any(|c| c == OUR_LAYER),
                "{pid}: a refused overlay is reported as a cover"
            );
        }
    }
}
