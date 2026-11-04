//! Window geometry from the compositor's own IPC (sway and Hyprland only).
//!
//! The foreign-toplevel protocols carry an app id, a title and state but no
//! position, size, output or stacking. sway (i3 IPC `GET_TREE`) and Hyprland
//! (`j/clients`, `j/monitors`, `j/layers`, `j/getoption`) report all of it, in
//! global logical coordinates - the space `screens` reports outputs in. This
//! module reads one consistent-enough snapshot of that state and matches it to
//! foreign-toplevel windows WITHOUT guessing: a toplevel gets IPC data only when
//! its app id + title pair is unique among the toplevels AND among the IPC
//! windows. Ambiguity leaves it without geometry. niri, river and every other
//! compositor have no reader here: `snapshot` fails and callers keep the
//! protocol-only behavior.
//!
//! Window rect semantics:
//! - sway: the container `rect` - borders included, title bar / tab bar
//!   excluded (sway's IPC subtracts the decoration; verified on sway 1.12).
//! - Hyprland: `at` + `size` - the client box, border excluded.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ax::{IpcKind, connect, hypr_socket, sway_get_tree, sway_socket, with_deadline};
use crate::output::Rect;

/// Where a window sits in the compositor's z-order, coarsely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Tiled,
    Floating,
    Fullscreen,
}

/// One window (view / client) as the IPC reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    /// Wayland app id; for XWayland clients the X class (what foreign-toplevel
    /// reports for them on both compositors). May be empty.
    pub app_id: String,
    pub title: String,
    pub pid: Option<u32>,
    /// Global logical rect; `None` when the IPC reports no usable size.
    pub rect: Option<Rect>,
    /// Output (connector) name of the window's workspace.
    pub output: Option<String>,
    /// Workspace key, unique per compositor (`sway:<name>` / `hypr:<id>`).
    pub workspace: Option<String>,
    /// Drawn right now (sway `visible`; Hyprland: mapped, not hidden, on an
    /// active or open special workspace of a monitor, or pinned).
    pub visible: bool,
    pub layer: Layer,
    pub xwayland: bool,
    /// Back-to-front hint inside the compositor, higher = closer to the top.
    /// Only the order matters.
    pub stack: i64,
}

/// One IPC snapshot.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub kind: IpcKind,
    pub clients: Vec<Client>,
    /// Surfaces drawn above windows that are not windows: Hyprland layer-shell
    /// surfaces on the `top` and `overlay` levels. sway exposes none.
    pub overlays: Vec<Rect>,
    /// Corner radius (logical px) applied to every window (Hyprland
    /// `decoration:rounding`); 0 when none or unknown.
    pub corner_radius: i32,
}

/// Which areas a masked capture cannot see and therefore cannot mask when
/// they are drawn over a kept window.
pub fn mask_gaps(kind: IpcKind) -> &'static [&'static str] {
    match kind {
        IpcKind::Sway => &["popups", "layer_shell_top_overlay", "xwayland_unmanaged"],
        IpcKind::Hyprland => &["popups"],
    }
}

// ---------------------------------------------------------------- parsing

fn i32_of(v: &Value, k: &str) -> Option<i32> {
    v.get(k)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
}

fn rect_obj(v: &Value) -> Option<Rect> {
    let r = Rect {
        x: i32_of(v, "x")?,
        y: i32_of(v, "y")?,
        width: i32_of(v, "width")?,
        height: i32_of(v, "height")?,
    };
    (r.width > 0 && r.height > 0).then_some(r)
}

fn pid_of(v: &Value) -> Option<u32> {
    v.get("pid")
        .and_then(Value::as_i64)
        .filter(|p| *p > 0)
        .and_then(|p| u32::try_from(p).ok())
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// Fixed stacking bases: fullscreen above floating above tiled.
fn layer_base(l: Layer) -> i64 {
    match l {
        Layer::Tiled => 0,
        Layer::Floating => 1_000_000,
        Layer::Fullscreen => 2_000_000,
    }
}

/// Parse a sway `GET_TREE` reply.
pub fn parse_sway(tree: &Value) -> Snapshot {
    let mut clients = Vec::new();
    walk_sway(tree, &Ctx::default(), &mut clients);
    Snapshot {
        kind: IpcKind::Sway,
        clients,
        overlays: Vec::new(),
        corner_radius: 0,
    }
}

#[derive(Default, Clone)]
struct Ctx {
    output: Option<String>,
    workspace: Option<String>,
    floating: bool,
    /// Index of the top-level floating container in its workspace (draw order).
    float_index: i64,
}

fn is_sway_view(n: &Value) -> bool {
    matches!(str_of(n, "type"), Some("con" | "floating_con"))
        && (n.get("shell").is_some_and(|s| s.is_string())
            || n.get("app_id").is_some_and(|s| s.is_string())
            || n.get("window_properties").is_some_and(|s| s.is_object()))
}

fn walk_sway(node: &Value, ctx: &Ctx, out: &mut Vec<Client>) {
    let mut ctx = ctx.clone();
    match str_of(node, "type") {
        Some("output") => {
            // `__i3` holds the scratchpad: no real output.
            ctx.output = str_of(node, "name")
                .filter(|n| !n.starts_with("__"))
                .map(str::to_owned);
        }
        Some("workspace") => {
            ctx.workspace = str_of(node, "name").map(|n| format!("sway:{n}"));
        }
        _ => {}
    }
    if is_sway_view(node) {
        let shell = str_of(node, "shell");
        let app_id = str_of(node, "app_id")
            .filter(|s| !s.is_empty())
            .or_else(|| {
                node.pointer("/window_properties/class")
                    .and_then(Value::as_str)
            })
            .unwrap_or_default();
        let fullscreen = node
            .get("fullscreen_mode")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > 0;
        let layer = if fullscreen {
            Layer::Fullscreen
        } else if ctx.floating {
            Layer::Floating
        } else {
            Layer::Tiled
        };
        out.push(Client {
            app_id: app_id.to_owned(),
            title: str_of(node, "name").unwrap_or_default().to_owned(),
            pid: pid_of(node),
            rect: node.get("rect").and_then(rect_obj),
            output: ctx.output.clone(),
            workspace: ctx.workspace.clone(),
            visible: node
                .get("visible")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            layer,
            xwayland: shell == Some("xwayland"),
            stack: layer_base(layer) + if ctx.floating { ctx.float_index } else { 0 },
        });
    }
    if let Some(children) = node.get("nodes").and_then(Value::as_array) {
        for child in children {
            walk_sway(child, &ctx, out);
        }
    }
    if let Some(children) = node.get("floating_nodes").and_then(Value::as_array) {
        for (i, child) in children.iter().enumerate() {
            let mut c = ctx.clone();
            if !ctx.floating {
                // Workspace floating list: later entries are drawn on top.
                c.float_index = i as i64 + 1;
            }
            c.floating = true;
            walk_sway(child, &c, out);
        }
    }
}

/// A Hyprland monitor as `j/monitors` reports it.
#[derive(Debug, Clone)]
struct HyprMonitor {
    id: i64,
    name: String,
    active_ws: Option<i64>,
    special_ws: Option<i64>,
}

fn parse_hypr_monitors(v: &Value) -> Vec<HyprMonitor> {
    let ws_id = |m: &Value, k: &str| {
        m.get(k)
            .and_then(|w| w.get("id"))
            .and_then(Value::as_i64)
            .filter(|id| *id != 0)
    };
    v.as_array()
        .map(|list| {
            list.iter()
                .filter(|m| !m.get("disabled").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|m| {
                    Some(HyprMonitor {
                        id: m.get("id")?.as_i64()?,
                        name: str_of(m, "name")?.to_owned(),
                        active_ws: ws_id(m, "activeWorkspace"),
                        special_ws: ws_id(m, "specialWorkspace"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Hyprland `fullscreen`: an int (0 none, 1 maximized, 2 fullscreen, 3 both)
/// on current releases, a bool on old ones. Any non-zero value counts.
fn hypr_fullscreen(c: &Value) -> bool {
    match c.get("fullscreen") {
        Some(Value::Bool(b)) => *b,
        Some(v) => v.as_i64().unwrap_or(0) != 0,
        None => false,
    }
}

/// Parse Hyprland `j/clients` + `j/monitors` (+ `j/layers`, `decoration:rounding`).
pub fn parse_hypr(
    clients: &Value,
    monitors: &Value,
    layers: Option<&Value>,
    rounding: Option<i32>,
) -> Snapshot {
    let mons = parse_hypr_monitors(monitors);
    let shown: Vec<i64> = mons
        .iter()
        .flat_map(|m| [m.active_ws, m.special_ws])
        .flatten()
        .collect();
    let out = clients
        .as_array()
        .map(|list| {
            list.iter()
                .map(|c| {
                    let pair = |k: &str| {
                        let a = c.get(k)?.as_array()?;
                        Some((
                            i32::try_from(a.first()?.as_i64()?).ok()?,
                            i32::try_from(a.get(1)?.as_i64()?).ok()?,
                        ))
                    };
                    let rect = match (pair("at"), pair("size")) {
                        (Some((x, y)), Some((width, height))) if width > 0 && height > 0 => {
                            Some(Rect {
                                x,
                                y,
                                width,
                                height,
                            })
                        }
                        _ => None,
                    };
                    let ws = c.pointer("/workspace/id").and_then(Value::as_i64);
                    let mapped = c.get("mapped").and_then(Value::as_bool).unwrap_or(true);
                    let hidden = c.get("hidden").and_then(Value::as_bool).unwrap_or(false);
                    let pinned = c.get("pinned").and_then(Value::as_bool).unwrap_or(false);
                    let on_shown = ws.is_some_and(|id| shown.contains(&id));
                    let special = ws.is_some_and(|id| id < 0);
                    let layer = if hypr_fullscreen(c) {
                        Layer::Fullscreen
                    } else if c.get("floating").and_then(Value::as_bool).unwrap_or(false) {
                        Layer::Floating
                    } else {
                        Layer::Tiled
                    };
                    let output = match c.get("monitor") {
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(v) => v
                            .as_i64()
                            .and_then(|id| mons.iter().find(|m| m.id == id))
                            .map(|m| m.name.clone()),
                        None => None,
                    };
                    // focusHistoryID: 0 = focused most recently.
                    let recency = c
                        .get("focusHistoryID")
                        .and_then(Value::as_i64)
                        .filter(|n| *n >= 0)
                        .map_or(0, |n| 100_000 - n.min(99_999));
                    Client {
                        app_id: str_of(c, "class").unwrap_or_default().to_owned(),
                        title: str_of(c, "title").unwrap_or_default().to_owned(),
                        pid: pid_of(c),
                        rect,
                        output,
                        workspace: ws.map(|id| format!("hypr:{id}")),
                        visible: mapped && !hidden && (on_shown || pinned),
                        layer,
                        xwayland: c.get("xwayland").and_then(Value::as_bool).unwrap_or(false),
                        stack: layer_base(layer) + if special { 500_000 } else { 0 } + recency,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let mut overlays = Vec::new();
    if let Some(map) = layers.and_then(Value::as_object) {
        for mon in map.values() {
            for level in ["2", "3"] {
                let Some(list) = mon
                    .pointer(&format!("/levels/{level}"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for l in list {
                    let r = Rect {
                        x: i32_of(l, "x").unwrap_or(0),
                        y: i32_of(l, "y").unwrap_or(0),
                        width: i32_of(l, "w").unwrap_or(0),
                        height: i32_of(l, "h").unwrap_or(0),
                    };
                    if r.width > 0 && r.height > 0 {
                        overlays.push(r);
                    }
                }
            }
        }
    }
    Snapshot {
        kind: IpcKind::Hyprland,
        clients: out,
        overlays,
        corner_radius: rounding.unwrap_or(0).max(0),
    }
}

// -------------------------------------------------------------------- IPC

/// One Hyprland request (`j/clients`, ...) on a fresh socket connection.
fn hypr_request(path: &Path, cmd: &str) -> Result<Value> {
    let mut s = connect(path).context("connect Hyprland socket")?;
    s.write_all(cmd.as_bytes())?;
    let mut body = Vec::new();
    s.take(64 << 20).read_to_end(&mut body)?;
    serde_json::from_slice(&body).with_context(|| format!("Hyprland `{cmd}` reply is not JSON"))
}

/// Read a snapshot, no deadline. `for_mask` also reads what masking needs on
/// Hyprland (layer surfaces, rounding) and fails when any of it is missing.
fn snapshot_blocking(for_mask: bool) -> Result<Snapshot> {
    if let Some(p) = sway_socket() {
        let tree = sway_get_tree(&p).context("sway GET_TREE")?;
        return Ok(parse_sway(&tree));
    }
    if let Some(p) = hypr_socket() {
        let clients = hypr_request(&p, "j/clients")?;
        let monitors = hypr_request(&p, "j/monitors")?;
        anyhow::ensure!(
            clients.is_array() && monitors.is_array(),
            "unexpected Hyprland clients/monitors reply"
        );
        if !for_mask {
            return Ok(parse_hypr(&clients, &monitors, None, None));
        }
        let layers = hypr_request(&p, "j/layers")?;
        anyhow::ensure!(layers.is_object(), "unexpected Hyprland layers reply");
        let rounding = hypr_request(&p, "j/getoption decoration:rounding")?
            .get("int")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .context("Hyprland decoration:rounding has no int value")?;
        return Ok(parse_hypr(
            &clients,
            &monitors,
            Some(&layers),
            Some(rounding),
        ));
    }
    bail!("no window-geometry IPC in this session (only sway and Hyprland are supported)")
}

/// [`snapshot_blocking`] under a wall-clock limit, so a wedged compositor can
/// never hang the command.
pub fn snapshot(for_mask: bool, limit: Duration) -> Result<Snapshot> {
    match with_deadline(limit, move || Some(snapshot_blocking(for_mask))) {
        Some(r) => r,
        None => bail!(
            "compositor IPC did not answer within {} ms",
            limit.as_millis()
        ),
    }
}

// --------------------------------------------------------------- matching

/// The (app id, title) key a toplevel and an IPC client are matched on.
pub fn key<'a>(app_id: Option<&'a str>, title: Option<&'a str>) -> (&'a str, &'a str) {
    (app_id.unwrap_or_default(), title.unwrap_or_default())
}

/// For each toplevel key, the index of its IPC client when the key occurs
/// exactly once among the toplevels and exactly once among the clients. An
/// all-empty key never matches.
pub fn match_unique(toplevels: &[(&str, &str)], clients: &[Client]) -> Vec<Option<usize>> {
    toplevels
        .iter()
        .map(|k| {
            if k.0.is_empty() && k.1.is_empty() {
                return None;
            }
            if toplevels.iter().filter(|o| *o == k).count() != 1 {
                return None;
            }
            let mut hits = clients
                .iter()
                .enumerate()
                .filter(|(_, c)| c.app_id == k.0 && c.title == k.1);
            let (i, _) = hits.next()?;
            hits.next().is_none().then_some(i)
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn r(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// sway, three outputs: HEADLESS-1 1280x800 at 0,0 (scale 1), HEADLESS-2
    /// 1280x720 logical at 1280,0 (scale 1.5), HEADLESS-3 720x1280 at 0,800
    /// (transform 90). Shapes copied from a live sway 1.12 GET_TREE.
    pub(crate) fn sway_fixture() -> Value {
        json!({"type":"root","name":"root","rect":{"x":0,"y":0,"width":2560,"height":2080},"nodes":[
          {"type":"output","name":"__i3","nodes":[
            {"type":"workspace","name":"__i3_scratch","nodes":[],"floating_nodes":[
              {"type":"floating_con","name":"scratch","app_id":"scratchy","pid":5,"shell":"xdg_shell",
               "visible":false,"fullscreen_mode":0,"rect":{"x":10,"y":10,"width":300,"height":200},
               "nodes":[],"floating_nodes":[]}]}]},
          {"type":"output","name":"HEADLESS-1","rect":{"x":0,"y":0,"width":1280,"height":800},"nodes":[
            {"type":"workspace","name":"1","nodes":[
              {"type":"con","name":"Editor","app_id":"org.gnome.TextEditor","pid":100,"shell":"xdg_shell",
               "visible":true,"fullscreen_mode":0,"rect":{"x":0,"y":0,"width":640,"height":800},
               "window_rect":{"x":2,"y":2,"width":636,"height":796},"nodes":[],"floating_nodes":[]},
              {"type":"con","name":null,"layout":"tabbed","rect":{"x":640,"y":0,"width":640,"height":800},
               "nodes":[
                {"type":"con","name":"Docs","app_id":"firefox","pid":200,"shell":"xdg_shell","visible":true,
                 "fullscreen_mode":0,"rect":{"x":640,"y":25,"width":640,"height":775},"nodes":[],"floating_nodes":[]},
                {"type":"con","name":"Mail","app_id":"firefox","pid":200,"shell":"xdg_shell","visible":false,
                 "fullscreen_mode":0,"rect":{"x":640,"y":25,"width":640,"height":775},"nodes":[],"floating_nodes":[]}
               ],"floating_nodes":[]}
            ],"floating_nodes":[
              {"type":"floating_con","name":"Calculator","app_id":"org.gnome.Calculator","pid":300,
               "shell":"xdg_shell","visible":true,"fullscreen_mode":0,
               "rect":{"x":500,"y":300,"width":400,"height":300},"nodes":[],"floating_nodes":[]},
              {"type":"floating_con","name":"xterm","app_id":null,"pid":400,"shell":"xwayland",
               "window_properties":{"class":"XTerm","instance":"xterm"},"visible":true,"fullscreen_mode":0,
               "rect":{"x":100,"y":100,"width":200,"height":100},"nodes":[],"floating_nodes":[]}
            ]},
            {"type":"workspace","name":"4","nodes":[
              {"type":"con","name":"Hidden","app_id":"foot","pid":500,"shell":"xdg_shell","visible":false,
               "fullscreen_mode":0,"rect":{"x":0,"y":0,"width":1280,"height":800},"nodes":[],"floating_nodes":[]}
            ],"floating_nodes":[]}
          ]},
          {"type":"output","name":"HEADLESS-2","rect":{"x":1280,"y":0,"width":1280,"height":720},"nodes":[
            {"type":"workspace","name":"2","nodes":[
              {"type":"con","name":"Video","app_id":"mpv","pid":600,"shell":"xdg_shell","visible":true,
               "fullscreen_mode":1,"rect":{"x":1280,"y":0,"width":1280,"height":720},"nodes":[],"floating_nodes":[]}
            ],"floating_nodes":[]}]},
          {"type":"output","name":"HEADLESS-3","rect":{"x":0,"y":800,"width":720,"height":1280},"nodes":[
            {"type":"workspace","name":"3","nodes":[
              {"type":"con","name":"Term","app_id":"foot","pid":700,"shell":"xdg_shell","visible":true,
               "fullscreen_mode":0,"rect":{"x":0,"y":800,"width":720,"height":1280},"nodes":[],"floating_nodes":[]}
            ],"floating_nodes":[]}]}
        ]})
    }

    #[test]
    fn sway_views_outputs_layers() {
        let s = parse_sway(&sway_fixture());
        let by = |t: &str| s.clients.iter().find(|c| c.title == t).unwrap();
        assert_eq!(s.clients.len(), 9);
        let ed = by("Editor");
        assert_eq!(ed.rect, Some(r(0, 0, 640, 800)));
        assert_eq!(ed.output.as_deref(), Some("HEADLESS-1"));
        assert_eq!(ed.workspace.as_deref(), Some("sway:1"));
        assert_eq!(ed.layer, Layer::Tiled);
        assert_eq!(ed.pid, Some(100));
        assert!(ed.visible);
        // Tabbed: the hidden tab is not visible; rects exclude the tab bar.
        assert!(by("Docs").visible);
        assert!(!by("Mail").visible);
        assert_eq!(by("Docs").rect, Some(r(640, 25, 640, 775)));
        // Floating + XWayland: class stands in for the app id.
        let x = by("xterm");
        assert_eq!(x.app_id, "XTerm");
        assert!(x.xwayland);
        assert_eq!(x.layer, Layer::Floating);
        assert!(x.stack > by("Calculator").stack, "later floating is on top");
        assert!(by("Calculator").stack > ed.stack);
        // Fullscreen on the scaled output; scratchpad has no output.
        assert_eq!(by("Video").layer, Layer::Fullscreen);
        assert_eq!(by("Video").output.as_deref(), Some("HEADLESS-2"));
        assert_eq!(by("scratch").output, None);
        assert!(!by("scratch").visible);
        assert!(!by("Hidden").visible);
        assert_eq!(by("Term").output.as_deref(), Some("HEADLESS-3"));
        assert_eq!(s.overlays, vec![]);
    }

    /// Hyprland: DP-1 2560x1440 physical at 0,0 scale 1.5 (1706x960 logical),
    /// HDMI-A-1 1920x1080 at 1706,0 scale 1 transform 1. Special workspace
    /// -98 open on DP-1.
    pub(crate) fn hypr_fixture() -> (Value, Value, Value) {
        let monitors = json!([
          {"id":0,"name":"DP-1","width":2560,"height":1440,"x":0,"y":0,"scale":1.5,"transform":0,
           "activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":-98,"name":"special:magic"},"disabled":false},
          {"id":1,"name":"HDMI-A-1","width":1920,"height":1080,"x":1706,"y":0,"scale":1.0,"transform":1,
           "activeWorkspace":{"id":2,"name":"2"},"specialWorkspace":{"id":0,"name":""},"disabled":false}
        ]);
        let clients = json!([
          {"address":"0x1","mapped":true,"hidden":false,"at":[10,40],"size":[840,910],"workspace":{"id":1,"name":"1"},
           "floating":false,"monitor":0,"class":"kitty","title":"~","pid":11,"xwayland":false,"pinned":false,
           "fullscreen":0,"focusHistoryID":1},
          {"address":"0x2","mapped":true,"hidden":false,"at":[860,40],"size":[836,910],"workspace":{"id":1,"name":"1"},
           "floating":false,"monitor":0,"class":"firefox","title":"Docs","pid":22,"xwayland":false,"pinned":false,
           "fullscreen":0,"focusHistoryID":2},
          {"address":"0x3","mapped":true,"hidden":true,"at":[860,40],"size":[836,910],"workspace":{"id":1,"name":"1"},
           "floating":false,"monitor":0,"class":"firefox","title":"Mail","pid":22,"xwayland":false,"pinned":false,
           "fullscreen":0,"focusHistoryID":5},
          {"address":"0x4","mapped":true,"hidden":false,"at":[300,200],"size":[400,300],"workspace":{"id":1,"name":"1"},
           "floating":true,"monitor":0,"class":"steam","title":"Friends","pid":33,"xwayland":true,"pinned":false,
           "fullscreen":0,"focusHistoryID":0},
          {"address":"0x5","mapped":true,"hidden":false,"at":[100,100],"size":[600,400],"workspace":{"id":-98,"name":"special:magic"},
           "floating":false,"monitor":0,"class":"pavucontrol","title":"Volume","pid":44,"xwayland":false,"pinned":false,
           "fullscreen":0,"focusHistoryID":3},
          {"address":"0x6","mapped":true,"hidden":false,"at":[1706,0],"size":[1080,1920],"workspace":{"id":2,"name":"2"},
           "floating":false,"monitor":1,"class":"mpv","title":"Video","pid":55,"xwayland":false,"pinned":false,
           "fullscreen":2,"focusHistoryID":4},
          {"address":"0x7","mapped":true,"hidden":false,"at":[0,0],"size":[1706,960],"workspace":{"id":3,"name":"3"},
           "floating":false,"monitor":0,"class":"code","title":"Project","pid":66,"xwayland":false,"pinned":false,
           "fullscreen":0,"focusHistoryID":6},
          {"address":"0x8","mapped":false,"hidden":false,"at":[0,0],"size":[0,0],"workspace":{"id":-1,"name":""},
           "floating":false,"monitor":-1,"class":"ghost","title":"","pid":77,"xwayland":false,"pinned":false,
           "fullscreen":false,"focusHistoryID":-1}
        ]);
        let layers = json!({
          "DP-1":{"levels":{"0":[{"address":"0xa","x":0,"y":0,"w":1706,"h":960,"namespace":"wallpaper","pid":1}],
                            "1":[],
                            "2":[{"address":"0xb","x":0,"y":0,"w":1706,"h":30,"namespace":"waybar","pid":2}],
                            "3":[{"address":"0xc","x":1300,"y":40,"w":380,"h":90,"namespace":"notifications","pid":3}]}},
          "HDMI-A-1":{"levels":{"0":[],"1":[],"2":[],"3":[]}}
        });
        (clients, monitors, layers)
    }

    #[test]
    fn hypr_visibility_output_layers() {
        let (c, m, l) = hypr_fixture();
        let s = parse_hypr(&c, &m, Some(&l), Some(8));
        let by = |cls: &str, t: &str| {
            s.clients
                .iter()
                .find(|x| x.app_id == cls && x.title == t)
                .unwrap()
        };
        assert_eq!(s.clients.len(), 8);
        assert_eq!(s.corner_radius, 8);
        assert_eq!(s.overlays, vec![r(0, 0, 1706, 30), r(1300, 40, 380, 90)]);
        let k = by("kitty", "~");
        assert_eq!(k.rect, Some(r(10, 40, 840, 910)));
        assert_eq!(k.output.as_deref(), Some("DP-1"));
        assert!(k.visible);
        assert_eq!(k.layer, Layer::Tiled);
        // Hidden group member, inactive workspace, unmapped: not visible.
        assert!(!by("firefox", "Mail").visible);
        assert!(!by("code", "Project").visible);
        assert!(!by("ghost", "").visible);
        assert_eq!(by("ghost", "").rect, None);
        // Open special workspace is visible and stacks above the regular one.
        let sp = by("pavucontrol", "Volume");
        assert!(sp.visible);
        assert!(sp.stack > k.stack);
        // XWayland floating window, most recently focused.
        let st = by("steam", "Friends");
        assert!(st.xwayland);
        assert_eq!(st.layer, Layer::Floating);
        assert!(st.stack > k.stack);
        let v = by("mpv", "Video");
        assert_eq!(v.layer, Layer::Fullscreen);
        assert_eq!(v.output.as_deref(), Some("HDMI-A-1"));
        assert_eq!(v.workspace.as_deref(), Some("hypr:2"));
    }

    #[test]
    fn hypr_pinned_counts_as_visible() {
        let (_, m, _) = hypr_fixture();
        let c = json!([{"mapped":true,"hidden":false,"at":[5,5],"size":[50,50],"workspace":{"id":9,"name":"9"},
            "floating":true,"monitor":"DP-1","class":"pip","title":"Picture","pinned":true,"fullscreen":false}]);
        let s = parse_hypr(&c, &m, None, None);
        assert!(s.clients[0].visible);
        assert_eq!(s.clients[0].output.as_deref(), Some("DP-1"));
        assert_eq!(s.corner_radius, 0);
    }

    #[test]
    fn match_unique_rules() {
        let s = parse_sway(&sway_fixture());
        let tl = [
            ("org.gnome.TextEditor", "Editor"),
            ("firefox", "Docs"),
            ("XTerm", "xterm"),
            ("nope", "x"),
            ("", ""),
        ];
        let m = match_unique(&tl, &s.clients);
        assert_eq!(s.clients[m[0].unwrap()].title, "Editor");
        assert_eq!(s.clients[m[1].unwrap()].title, "Docs");
        assert_eq!(s.clients[m[2].unwrap()].app_id, "XTerm");
        assert_eq!(m[3], None);
        assert_eq!(m[4], None);
        // Ambiguous among toplevels.
        let tl = [("firefox", "Docs"), ("firefox", "Docs")];
        assert_eq!(match_unique(&tl, &s.clients), vec![None, None]);
        // Ambiguous among IPC clients.
        let mut dup = s.clients.clone();
        dup.push(dup.iter().find(|c| c.title == "Docs").unwrap().clone());
        assert_eq!(match_unique(&[("firefox", "Docs")], &dup), vec![None]);
    }
}
