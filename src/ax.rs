//! AT-SPI subcommands (`ax-find`, `ax-action`, `ax-enable`, `ax-restore`) and the
//! `doctor.atspi` key, wrapping the shared `cu-atspi` crate.
//!
//! The foreign-toplevel protocols give an app id and a title but neither a pid
//! nor a geometry. `ax-find` therefore probes the compositor's own IPC for
//! both, best effort and never required: sway (i3 IPC `GET_TREE`) and Hyprland
//! (`j/clients`). A window is enriched only when exactly one IPC window and
//! exactly one foreign-toplevel window carry its app id + title; ambiguity or
//! any IPC failure leaves the hint without pid and rect (AT-SPI then answers
//! `window_only`). Nothing here opens a portal or touches input/screenshots.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use cu_atspi::{FindRequest, Rect, WindowHint};
use serde_json::{Value, json};

use crate::conn::Conn;

/// Per-read/write socket timeout.
const IPC_TIMEOUT: Duration = Duration::from_millis(1500);
/// Overall budget for one IPC exchange, connect included.
const IPC_DEADLINE: Duration = Duration::from_millis(1500);
/// Largest accepted `set-text` payload on stdin.
pub const MAX_STDIN_TEXT: usize = 16 << 20;

/// `ax-find` flags after the window id is resolved.
#[derive(Debug, Default)]
pub struct FindOptions {
    pub role: Option<String>,
    pub title_contains: Option<String>,
    pub all: bool,
    pub include_hidden: bool,
    pub max_nodes: Option<usize>,
    pub max_depth: Option<u32>,
    pub expect_app_id: Option<String>,
    pub expect_title: Option<String>,
}

/// One window as the compositor IPC reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct IpcWindow {
    pub app_id: String,
    pub title: String,
    pub pid: Option<u32>,
    /// Window rect, global logical px. Sway: the decorated container. Hyprland:
    /// `at`/`size`, which exclude the border (so it equals the client rect).
    pub rect: Option<Rect>,
    /// Client rect, global logical px, when the IPC tells it.
    pub client_rect: Option<Rect>,
}

/// Which IPC enrichment source is usable right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcKind {
    Sway,
    Hyprland,
}

impl IpcKind {
    fn as_str(self) -> &'static str {
        match self {
            IpcKind::Sway => "sway",
            IpcKind::Hyprland => "hyprland",
        }
    }
}

// ---------------------------------------------------------------- parsing

fn rect_of(v: &Value) -> Option<Rect> {
    let n = |k: &str| v.get(k).and_then(Value::as_i64).map(|n| n as i32);
    Some(Rect::new(n("x")?, n("y")?, n("width")?, n("height")?))
}

/// Parse a sway `GET_TREE` reply into its application windows.
pub fn parse_sway_tree(tree: &Value) -> Vec<IpcWindow> {
    let mut out = Vec::new();
    walk_sway(tree, &mut out);
    out
}

fn walk_sway(node: &Value, out: &mut Vec<IpcWindow>) {
    let app_id = node
        .get("app_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        // XWayland windows have no app_id; the foreign-toplevel app id is their class.
        .or_else(|| {
            node.pointer("/window_properties/class")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    if let Some(app_id) = app_id {
        let rect = node.get("rect").and_then(rect_of);
        // `window_rect` is relative to the container's rect.
        let client_rect = match (rect, node.get("window_rect").and_then(rect_of)) {
            (Some(r), Some(w)) if w.width > 0 && w.height > 0 => {
                Some(Rect::new(r.x + w.x, r.y + w.y, w.width, w.height))
            }
            _ => None,
        };
        out.push(IpcWindow {
            app_id: app_id.to_owned(),
            title: node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            pid: node
                .get("pid")
                .and_then(Value::as_u64)
                .filter(|p| *p > 0)
                .map(|p| p as u32),
            rect: rect.filter(|r| r.width > 0 && r.height > 0),
            client_rect,
        });
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            for child in children {
                walk_sway(child, out);
            }
        }
    }
}

/// Parse Hyprland's `j/clients` reply.
pub fn parse_hypr_clients(clients: &Value) -> Vec<IpcWindow> {
    let Some(list) = clients.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter(|c| c.get("mapped").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|c| {
            let app_id = c.get("class").and_then(Value::as_str)?.to_owned();
            if app_id.is_empty() {
                return None;
            }
            let pair = |k: &str| {
                let a = c.get(k)?.as_array()?;
                Some((a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32))
            };
            let rect = match (pair("at"), pair("size")) {
                (Some((x, y)), Some((w, h))) if w > 0 && h > 0 => Some(Rect::new(x, y, w, h)),
                _ => None,
            };
            Some(IpcWindow {
                app_id,
                title: c
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                pid: c
                    .get("pid")
                    .and_then(Value::as_u64)
                    .filter(|p| *p > 0)
                    .map(|p| p as u32),
                rect,
                client_rect: rect,
            })
        })
        .collect()
}

// --------------------------------------------------------------- matching

/// The one IPC window carrying exactly this app id + title; `None` when there
/// is no such window or more than one (ambiguous: never guess).
pub fn unique_match<'a>(
    windows: &'a [IpcWindow],
    app_id: &str,
    title: &str,
) -> Option<&'a IpcWindow> {
    let mut hits = windows
        .iter()
        .filter(|w| w.app_id == app_id && w.title == title);
    let first = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    Some(first)
}

/// Fill pid and rects of `hint` from `windows` when the match is unambiguous on
/// both sides (`toplevel_dupes`: foreign-toplevel windows with the same app id
/// + title, this one included). Returns whether it enriched.
pub fn enrich(hint: &mut WindowHint, windows: &[IpcWindow], toplevel_dupes: usize) -> bool {
    if toplevel_dupes != 1 {
        return false;
    }
    let (Some(app_id), Some(title)) = (hint.app_id.clone(), hint.title.clone()) else {
        return false;
    };
    let Some(w) = unique_match(windows, &app_id, &title) else {
        return false;
    };
    hint.pid = w.pid;
    hint.rect_screen_logical = w.rect;
    hint.client_rect_screen_logical = w.client_rect;
    true
}

// -------------------------------------------------------------------- IPC

fn sway_socket() -> Option<PathBuf> {
    std::env::var_os("SWAYSOCK")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn hypr_socket() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .ok()
        .filter(|s| !s.is_empty() && !s.contains('/'))?;
    let mut candidates = Vec::new();
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        candidates.push(
            PathBuf::from(run)
                .join("hypr")
                .join(&sig)
                .join(".socket.sock"),
        );
    }
    // Older Hyprland releases.
    candidates.push(PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock"));
    candidates.into_iter().find(|p| p.exists())
}

fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(IPC_TIMEOUT))?;
    s.set_write_timeout(Some(IPC_TIMEOUT))?;
    Ok(s)
}

/// Frame an i3-ipc message: magic, payload length, type (native endian).
pub fn i3_frame(msg_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + payload.len());
    m.extend_from_slice(b"i3-ipc");
    m.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    m.extend_from_slice(&msg_type.to_ne_bytes());
    m.extend_from_slice(payload);
    m
}

fn sway_get_tree(path: &Path) -> Result<Value> {
    const GET_TREE: u32 = 4;
    let mut s = connect(path).context("connect SWAYSOCK")?;
    s.write_all(&i3_frame(GET_TREE, b""))?;
    let mut head = [0u8; 14];
    s.read_exact(&mut head)?;
    anyhow::ensure!(&head[..6] == b"i3-ipc", "bad i3-ipc magic");
    let len = u32::from_ne_bytes(head[6..10].try_into()?) as usize;
    let ty = u32::from_ne_bytes(head[10..14].try_into()?);
    anyhow::ensure!(ty == GET_TREE, "unexpected i3-ipc reply type {ty}");
    anyhow::ensure!(len <= 64 << 20, "i3-ipc reply too large");
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn hypr_get_clients(path: &Path) -> Result<Value> {
    let mut s = connect(path).context("connect Hyprland socket")?;
    s.write_all(b"j/clients")?;
    let mut body = Vec::new();
    s.take(64 << 20).read_to_end(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

/// One IPC exchange (connect + write + read), sway preferred, then Hyprland.
fn ipc_exchange() -> Option<(IpcKind, Vec<IpcWindow>)> {
    if let Some(p) = sway_socket()
        && let Ok(t) = sway_get_tree(&p)
    {
        return Some((IpcKind::Sway, parse_sway_tree(&t)));
    }
    if let Some(p) = hypr_socket()
        && let Ok(c) = hypr_get_clients(&p)
    {
        return Some((IpcKind::Hyprland, parse_hypr_clients(&c)));
    }
    None
}

/// Run `f` on a helper thread and give up after `limit` in total, so a wedged
/// compositor can never hang the command. The helper is abandoned on timeout.
fn with_deadline<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).ok().flatten()
}

fn ipc_snapshot(limit: Duration) -> Option<(IpcKind, Vec<IpcWindow>)> {
    with_deadline(limit, ipc_exchange)
}

// --------------------------------------------------------------- commands

/// Budget for the crate's doctor call; the launcher kills `doctor` at 5 s.
const DOCTOR_ATSPI_BUDGET: Duration = Duration::from_secs(3);
/// Total budget for `doctor.atspi` (crate call + IPC probe), under the 5 s kill.
const DOCTOR_TOTAL_BUDGET: Duration = Duration::from_millis(4500);

/// `doctor.atspi`: the crate's doctor plus `window_ipc`. Never fails and stays
/// under 4.5 s: the crate call is capped at 3 s, the IPC probe gets the rest.
pub fn doctor_atspi() -> Value {
    let start = std::time::Instant::now();
    let mut v = with_deadline(DOCTOR_ATSPI_BUDGET, || {
        serde_json::to_value(cu_atspi::blocking::doctor()).ok()
    })
    .unwrap_or_else(|| json!({"bus": false, "error": "accessibility doctor timed out"}));
    let left = DOCTOR_TOTAL_BUDGET
        .saturating_sub(start.elapsed())
        .clamp(Duration::from_millis(200), IPC_DEADLINE);
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "window_ipc".into(),
            json!(ipc_snapshot(left).map(|(k, _)| k.as_str())),
        );
    }
    v
}

/// Normalize an app id like the JS side: trim, drop `.desktop`, lowercase.
fn norm_app_id(s: &str) -> String {
    let t = s.trim();
    t.strip_suffix(".desktop").unwrap_or(t).to_lowercase()
}

/// Does the resolved toplevel still match what the caller listed? Window ids
/// are per-connection protocol ids in enumeration order, so a window opening
/// between `windows` and `ax-find` can shift them. Absent expectations pass.
pub fn expectation_holds(
    app_id: Option<&str>,
    title: Option<&str>,
    expect_app_id: Option<&str>,
    expect_title: Option<&str>,
) -> bool {
    if let Some(e) = expect_app_id
        && norm_app_id(e) != norm_app_id(app_id.unwrap_or_default())
    {
        return false;
    }
    if let Some(e) = expect_title
        && e != title.unwrap_or_default()
    {
        return false;
    }
    true
}

/// `ax-find`: resolve the window from the foreign-toplevel list, enrich it from
/// the compositor IPC when possible, then ask AT-SPI.
pub fn find(conn: &Conn, window: &str, opts: FindOptions) -> Result<Value> {
    let toplevels = crate::wm::collect_toplevels(conn)?;
    let Some(t) = toplevels.iter().find(|t| t.id == window) else {
        return Ok(json!({
            "unavailable": "window_not_found",
            "hint": format!("no window with id `{window}`; call windows for current ids"),
        }));
    };
    // Ids are not stable across connections (and the stable ext identifier is
    // not what `windows` prints), so the caller passes what it listed and a
    // shifted id is refused instead of acting on another app.
    if !expectation_holds(
        t.app_id.as_deref(),
        t.title.as_deref(),
        opts.expect_app_id.as_deref(),
        opts.expect_title.as_deref(),
    ) {
        return Ok(json!({
            "unavailable": "window_changed",
            "hint": "the window list changed; call app_list_windows again",
        }));
    }
    let mut hint = WindowHint {
        id: Some(t.id.clone()),
        app_id: t.app_id.clone().filter(|s| !s.is_empty()),
        title: t.title.clone(),
        x11: false,
        ..Default::default()
    };
    let dupes = toplevels
        .iter()
        .filter(|o| o.app_id == t.app_id && o.title == t.title)
        .count();
    if let Some((_, windows)) = ipc_snapshot(IPC_DEADLINE) {
        enrich(&mut hint, &windows, dupes);
    }
    let req = FindRequest {
        window: hint,
        role: opts.role,
        title_contains: opts.title_contains,
        all: opts.all,
        include_hidden: opts.include_hidden,
        max_nodes: opts.max_nodes,
        max_depth: opts.max_depth,
    };
    Ok(serde_json::to_value(cu_atspi::blocking::find(&req)?)?)
}

/// Caller error (bad input): the bridge exits 2 instead of 1.
#[derive(Debug)]
pub struct UsageError(pub &'static str);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UsageError {}

/// Read all of `r` as UTF-8, failing above `cap` bytes.
pub fn read_capped(r: impl Read, cap: usize) -> Result<String> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1).read_to_end(&mut buf)?;
    anyhow::ensure!(buf.len() <= cap, "stdin text exceeds {cap} bytes");
    String::from_utf8(buf).map_err(|_| anyhow::Error::new(UsageError("stdin is not valid UTF-8")))
}

/// Whole stdin as UTF-8, untrimmed, capped at 16 MiB. Never logged or echoed.
pub fn read_stdin_text() -> Result<String> {
    read_capped(std::io::stdin().lock(), MAX_STDIN_TEXT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sway_fixture() -> Value {
        json!({
        "type":"root","name":"root","nodes":[
          {"type":"output","name":"HEADLESS-1","nodes":[
            {"type":"workspace","name":"1","nodes":[
              {"type":"con","name":"Untitled 1 - Mousepad","app_id":"org.xfce.mousepad","pid":4242,
               "rect":{"x":0,"y":20,"width":800,"height":580},
               "window_rect":{"x":2,"y":0,"width":796,"height":578},"nodes":[],"floating_nodes":[]},
              {"type":"con","name":"term","app_id":"foot","pid":10,
               "rect":{"x":800,"y":0,"width":400,"height":600},
               "window_rect":{"x":0,"y":0,"width":400,"height":600},"nodes":[],"floating_nodes":[]}
            ],"floating_nodes":[
              {"type":"floating_con","name":"x11 app","app_id":null,"pid":77,
               "window_properties":{"class":"XTerm"},
               "rect":{"x":10,"y":10,"width":100,"height":50},
               "window_rect":{"x":0,"y":0,"width":0,"height":0},"nodes":[]}
            ]}
          ]}
        ]})
    }

    #[test]
    fn sway_tree_yields_windows() {
        let w = parse_sway_tree(&sway_fixture());
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].app_id, "org.xfce.mousepad");
        assert_eq!(w[0].pid, Some(4242));
        assert_eq!(w[0].rect, Some(Rect::new(0, 20, 800, 580)));
        assert_eq!(w[0].client_rect, Some(Rect::new(2, 20, 796, 578)));
        // XWayland: class stands in for app_id; empty window_rect gives no client rect.
        assert_eq!(w[2].app_id, "XTerm");
        assert_eq!(w[2].client_rect, None);
    }

    #[test]
    fn hypr_clients_parse() {
        let c = json!([
          {"address":"0x1","mapped":true,"at":[100,50],"size":[800,600],"class":"org.xfce.mousepad",
           "title":"Untitled 1 - Mousepad","pid":4242},
          {"address":"0x2","mapped":false,"at":[0,0],"size":[10,10],"class":"ghost","title":"g","pid":1},
          {"address":"0x3","mapped":true,"at":[0,0],"size":[0,0],"class":"nosize","title":"n","pid":0}
        ]);
        let w = parse_hypr_clients(&c);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].rect, Some(Rect::new(100, 50, 800, 600)));
        assert_eq!(w[0].pid, Some(4242));
        assert_eq!(w[1].rect, None);
        assert_eq!(w[1].pid, None);
        assert_eq!(w[0].client_rect, w[0].rect);
        assert!(parse_hypr_clients(&json!({"x":1})).is_empty());
    }

    fn hint(app: &str, title: &str) -> WindowHint {
        WindowHint {
            app_id: Some(app.into()),
            title: Some(title.into()),
            ..Default::default()
        }
    }

    #[test]
    fn unique_match_enriches() {
        let w = parse_sway_tree(&sway_fixture());
        let mut h = hint("org.xfce.mousepad", "Untitled 1 - Mousepad");
        assert!(enrich(&mut h, &w, 1));
        assert_eq!(h.pid, Some(4242));
        assert_eq!(h.rect_screen_logical, Some(Rect::new(0, 20, 800, 580)));
        assert_eq!(
            h.client_rect_screen_logical,
            Some(Rect::new(2, 20, 796, 578))
        );
    }

    #[test]
    fn no_match_does_not_enrich() {
        let w = parse_sway_tree(&sway_fixture());
        let mut h = hint("org.xfce.mousepad", "Other title");
        assert!(!enrich(&mut h, &w, 1));
        assert_eq!(h.pid, None);
        assert_eq!(h.rect_screen_logical, None);
    }

    #[test]
    fn ipc_ambiguity_does_not_enrich() {
        let mut w = parse_sway_tree(&sway_fixture());
        w.push(w[0].clone());
        let mut h = hint("org.xfce.mousepad", "Untitled 1 - Mousepad");
        assert!(unique_match(&w, "org.xfce.mousepad", "Untitled 1 - Mousepad").is_none());
        assert!(!enrich(&mut h, &w, 1));
        assert_eq!(h.pid, None);
    }

    #[test]
    fn toplevel_ambiguity_does_not_enrich() {
        let w = parse_sway_tree(&sway_fixture());
        let mut h = hint("org.xfce.mousepad", "Untitled 1 - Mousepad");
        assert!(!enrich(&mut h, &w, 2));
        assert_eq!(h.pid, None);
    }

    #[test]
    fn missing_app_id_or_title_does_not_enrich() {
        let w = parse_sway_tree(&sway_fixture());
        let mut h = WindowHint::default();
        assert!(!enrich(&mut h, &w, 1));
    }

    #[test]
    fn expectation_match_and_mismatch() {
        let h = expectation_holds;
        assert!(h(Some("Firefox"), Some("T"), None, None));
        assert!(h(
            Some("org.x.App"),
            Some("T"),
            Some("ORG.X.APP.desktop"),
            Some("T")
        ));
        assert!(!h(Some("foot"), Some("T"), Some("firefox"), None));
        assert!(!h(Some("foot"), Some("T"), None, Some("other")));
        assert!(!h(None, Some("T"), Some("foot"), None));
        assert!(h(None, None, Some(""), Some("")));
    }

    #[test]
    fn stdin_cap() {
        assert_eq!(read_capped(&b"abc"[..], 3).unwrap(), "abc");
        assert!(read_capped(&b"abcd"[..], 3).is_err());
        assert_eq!(read_capped(&b""[..], 3).unwrap(), "");
        let e = read_capped(&[0xffu8, 0xfe][..], 8).unwrap_err();
        assert!(e.downcast_ref::<UsageError>().is_some());
    }

    #[test]
    fn wedged_ipc_times_out() {
        let dir = std::env::temp_dir().join(format!("wlr-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let c = l.accept();
            std::thread::sleep(Duration::from_millis(1500));
            drop(c);
        });
        let start = std::time::Instant::now();
        let got = with_deadline(Duration::from_millis(300), move || {
            sway_get_tree(&sock).ok()
        });
        assert!(got.is_none());
        assert!(start.elapsed() < Duration::from_millis(1000));
        let _ = t.join();
    }

    #[test]
    fn i3_frame_layout() {
        let f = i3_frame(4, b"");
        assert_eq!(&f[..6], b"i3-ipc");
        assert_eq!(&f[6..10], &0u32.to_ne_bytes());
        assert_eq!(&f[10..14], &4u32.to_ne_bytes());
        assert_eq!(f.len(), 14);
    }
}
