//! `overlay` against a REAL headless sway (skipped when `sway` is not
//! installed, unless `WLROOTS_BRIDGE_REQUIRE_SWAY=1`). Private instance only:
//! own short XDG_RUNTIME_DIR, `WLR_BACKENDS=headless`, no input devices,
//! pixman renderer. Screenshots are taken with the bridge itself.
//!
//! `WLB_EVIDENCE_DIR=<dir>` keeps every screenshot (JPEG, as the bridge
//! returned it) for inspection.
//!
//! Needs `foot` for the window tests (two tiled windows side by side); with
//! `WLROOTS_BRIDGE_REQUIRE_SWAY=1` a missing foot fails instead of skipping.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use common::{BIN, TempDir, wait_for};
use serde_json::Value;

const T: Duration = Duration::from_secs(10);

fn have(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn required() -> bool {
    std::env::var("WLROOTS_BRIDGE_REQUIRE_SWAY").is_ok_and(|v| v == "1")
}

macro_rules! require {
    ($cmd:expr) => {
        if !have($cmd) {
            if required() {
                panic!(
                    "{} is required (WLROOTS_BRIDGE_REQUIRE_SWAY=1) but not installed",
                    $cmd
                );
            }
            eprintln!("{} not installed: skipping", $cmd);
            return;
        }
    };
}

// ------------------------------------------------------------------ sway

struct Sway {
    rt: TempDir,
    child: Child,
}

impl Sway {
    fn start() -> Self {
        let rt = TempDir::new("ov");
        let cfg = rt.path().join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x800 position 0 0\n\
             default_border none\nfocus_follows_mouse no\n",
        )
        .unwrap();
        let mut cmd = Command::new("sway");
        cmd.arg("-c")
            .arg(&cfg)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", rt.path())
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env("SWAYSOCK", rt.path().join("sway.sock"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(rt.path().join("sway.log")).unwrap());
        // foot needs a home for fontconfig's cache; nothing is written there
        // by sway itself.
        if let Some(h) = std::env::var_os("HOME") {
            cmd.env("HOME", h);
        }
        let child = cmd.spawn().expect("start sway");
        let s = Self { rt, child };
        let ok = wait_for(T, || {
            s.sock().exists() && s.rt.path().join("wayland-1").exists()
        });
        assert!(ok, "sway did not start: {}", s.log());
        std::thread::sleep(Duration::from_millis(300));
        s
    }

    fn sock(&self) -> PathBuf {
        self.rt.path().join("sway.sock")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.rt.path().join("sway.log")).unwrap_or_default()
    }

    /// The environment every bridge command gets: this sway only.
    fn env(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", self.rt.path())
            .env("WAYLAND_DISPLAY", "wayland-1")
            .env("SWAYSOCK", self.sock());
    }

    fn swaymsg(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("swaymsg");
        self.env(&mut cmd);
        let out = cmd.args(args).output().unwrap();
        assert!(out.status.success(), "swaymsg {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// One bridge command; its single JSON value.
    fn bridge(&self, args: &[&str]) -> Value {
        self.bridge_env(args, &[])
    }

    /// [`Sway::bridge`] with extra environment variables.
    fn bridge_env(&self, args: &[&str], extra: &[(&str, &str)]) -> Value {
        let mut cmd = Command::new(BIN);
        self.env(&mut cmd);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: stdout is not one JSON value"))
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------------------- overlay

/// A running `wlroots-bridge overlay`, stdin/stdout piped to the test.
struct Overlay {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Overlay {
    fn spawn(sway: &Sway, extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(BIN);
        sway.env(&mut cmd);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .arg("overlay")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let lines = reader(child.stdout.take().unwrap());
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{line}").unwrap();
        s.flush().unwrap();
    }

    fn wait_exit(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("overlay did not exit within {limit:?}");
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// stdout lines on a channel; every line must be one JSON object.
fn reader(out: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// The next stdout line with `"event":"<ev>"` (others are skipped, but each
/// must be a JSON object: stdout carries protocol lines only).
fn expect_event(rx: &mpsc::Receiver<String>, ev: &str) -> Value {
    let deadline = Instant::now() + T;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no `{ev}` event within {T:?}"));
        let v: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("overlay stdout line is not JSON: {line}"));
        assert!(v.is_object() && v.get("event").is_some(), "{line}");
        if v["event"] == ev {
            return v;
        }
        assert_ne!(v["event"], "error", "unexpected error event: {line}");
    }
}

fn gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(s) => s
            .rsplit(')')
            .next()
            .and_then(|r| r.split_whitespace().next())
            .is_some_and(|st| st == "Z" || st == "X"),
    }
}

// ----------------------------------------------------------- screenshots

struct Shot {
    img: image::RgbImage,
}

impl Shot {
    fn px(&self, x: u32, y: u32) -> [u8; 3] {
        self.img.get_pixel(x, y).0
    }
    fn count(&self, x0: u32, y0: u32, x1: u32, y1: u32, f: fn([u8; 3]) -> bool) -> usize {
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| f(self.px(x, y)))
            .count()
    }
}

fn evidence(name: &str, jpeg: &[u8]) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("WLB_EVIDENCE_DIR")?);
    std::fs::create_dir_all(&dir).ok()?;
    let p = dir.join(format!("{name}.jpg"));
    std::fs::write(&p, jpeg).ok()?;
    Some(p)
}

/// `screenshot` (plus `extra` flags), decoded. The output is 1280x800 at
/// scale 1, so image pixels are logical pixels.
fn shot(sway: &Sway, name: &str, extra: &[&str]) -> (Shot, Value) {
    shot_env(sway, name, extra, &[])
}

/// [`shot`] with extra environment variables (`WLROOTS_BRIDGE_MASK_CAPTURE`).
fn shot_env(sway: &Sway, name: &str, extra: &[&str], env: &[(&str, &str)]) -> (Shot, Value) {
    let mut args = vec!["screenshot"];
    args.extend_from_slice(extra);
    let v = sway.bridge_env(&args, env);
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(v["base64"].as_str().unwrap())
        .unwrap();
    if let Some(p) = evidence(name, &jpeg) {
        eprintln!("evidence: {}", p.display());
    }
    let img = image::load_from_memory(&jpeg).unwrap().to_rgb8();
    assert_eq!((img.width(), img.height()), (1280, 800));
    (Shot { img }, v)
}

fn clay(p: [u8; 3]) -> bool {
    p[0] > 150 && (70..160).contains(&p[1]) && p[2] < 130 && i32::from(p[0]) - i32::from(p[2]) > 70
}
fn white(p: [u8; 3]) -> bool {
    p.iter().all(|&c| c > 200)
}
fn near(p: [u8; 3], c: [u8; 3], tol: i32) -> bool {
    p.iter()
        .zip(c)
        .all(|(&a, b)| (i32::from(a) - i32::from(b)).abs() <= tol)
}
/// The mask fill #1E1E1E.
fn masked(p: [u8; 3]) -> bool {
    near(p, [0x1E, 0x1E, 0x1E], 6)
}
/// foot's background in the test config.
const FOOT_BG: [u8; 3] = [0x20, 0x40, 0x60];
fn foot_bg(p: [u8; 3]) -> bool {
    near(p, FOOT_BG, 10)
}

const SHOW_RIM: &str =
    r#"{"cmd":"show","output":"HEADLESS-1","look":"rim","arrow":{"x":300,"y":200}}"#;

/// Rim on all four edges, the arrow at (300,200), interior untouched.
fn assert_rim_and_arrow(s: &Shot, interior: fn([u8; 3]) -> bool) {
    for (x, y) in [(0, 400), (1, 400), (1279, 400), (640, 0), (640, 799)] {
        assert!(clay(s.px(x, y)), "rim at {x},{y}: {:?}", s.px(x, y));
    }
    assert!(clay(s.px(312, 212)), "arrow body: {:?}", s.px(312, 212));
    assert!(s.count(296, 195, 304, 203, white) > 4, "arrow outline");
    assert!(interior(s.px(640, 400)), "interior: {:?}", s.px(640, 400));
}

// ----------------------------------------------------------------- tests

#[test]
fn doctor_reports_overlay_on_sway() {
    require!("sway");
    let sway = Sway::start();
    let d = sway.bridge(&["doctor"]);
    assert_eq!(d["capabilities"]["overlay"], true, "{d}");
    // sway 1.12 advertises both; older sways may not (not asserted there).
    assert!(d["capabilities"]["overlay_alpha_modifier"].is_boolean());
    assert!(d["capabilities"]["overlay_fractional_scale"].is_boolean());
    // Backwards compatible: the old keys are still there.
    for k in [
        "contract",
        "session_match",
        "globals",
        "screenshot_mask",
        "abortWatch",
    ] {
        assert!(d.get(k).is_some(), "{k} missing: {d}");
    }
    eprintln!("doctor capabilities: {}", d["capabilities"]);
}

#[test]
fn overlay_draws_and_exits_on_stdin_eof() {
    require!("sway");
    let sway = Sway::start();
    let mut ov = Overlay::spawn(&sway, &[]);
    let ready = expect_event(&ov.lines, "ready");
    assert_eq!(ready["protocol"], 1);
    assert_eq!(ready["outputs"][0]["name"], "HEADLESS-1");
    ov.send(SHOW_RIM);
    expect_event(&ov.lines, "shown");
    std::thread::sleep(Duration::from_millis(500));
    let (s, _) = shot(&sway, "overlay-rim", &[]);
    assert_rim_and_arrow(&s, |p| p.iter().all(|&c| c <= 24));

    // stdin EOF: exit 0, surfaces gone with the connection.
    ov.stdin.take();
    assert_eq!(ov.wait_exit(T), Some(0));
    std::thread::sleep(Duration::from_millis(200));
    let (s, _) = shot(&sway, "overlay-after-eof", &[]);
    assert!(
        !clay(s.px(0, 400)) && !clay(s.px(312, 212)),
        "rim left behind"
    );
}

#[test]
fn overlay_dies_with_its_parent() {
    require!("sway");
    let sway = Sway::start();

    // 1. Direct parent killed (SIGKILL, no cleanup): PR_SET_PDEATHSIG. The
    //    shell keeps the overlay's stdin open (fd 3 = the test's pipe), so
    //    this is not an EOF exit.
    let mut sh = Command::new("sh");
    sway.env(&mut sh);
    let mut sh = sh
        .args([
            "-c",
            r#"exec 3<&0; "$0" overlay 0<&3 & echo "$!"; wait"#,
            BIN,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _keep_stdin = sh.stdin.take();
    let lines = reader(sh.stdout.take().unwrap());
    let pid: u32 = lines.recv_timeout(T).unwrap().trim().parse().unwrap();
    expect_event(&lines, "ready");
    assert!(!gone(pid));
    sh.kill().unwrap();
    sh.wait().unwrap();
    assert!(
        wait_for(T, || gone(pid)),
        "overlay {pid} survived its parent"
    );

    // 2. CLAUDE_CU_PARENT_PID names another process: its death ends the
    //    overlay with exit 0 although stdin stays open.
    let mut app = Command::new("sleep").arg("600").spawn().unwrap();
    let mut ov = Overlay::spawn(&sway, &[("CLAUDE_CU_PARENT_PID", &app.id().to_string())]);
    expect_event(&ov.lines, "ready");
    ov.send(SHOW_RIM);
    expect_event(&ov.lines, "shown");
    std::thread::sleep(Duration::from_millis(300));
    app.kill().unwrap();
    app.wait().unwrap();
    assert_eq!(ov.wait_exit(T), Some(0));
    std::thread::sleep(Duration::from_millis(200));
    let (s, _) = shot(&sway, "overlay-after-parent-death", &[]);
    assert!(!clay(s.px(0, 400)), "rim left behind after the parent died");
}

/// Two foot windows side by side; returns (left id, right id).
fn two_windows(sway: &Sway) -> (String, String) {
    // The colour section is `colors` before foot 1.24 and `colors-dark`
    // since; the other override only prints an error line at the top.
    let colors = "-o colors.background=204060 -o colors-dark.background=204060";
    sway.swaymsg(&[&format!(
        "exec foot -c /dev/null {colors} -T left-win sleep 600"
    )]);
    assert!(wait_for(T, || sway
        .swaymsg(&["-t", "get_tree"])
        .contains("left-win")));
    sway.swaymsg(&[&format!(
        "exec foot -c /dev/null {colors} -T right-win sleep 600"
    )]);
    assert!(wait_for(T, || sway
        .swaymsg(&["-t", "get_tree"])
        .contains("right-win")));
    std::thread::sleep(Duration::from_millis(800));
    let w = sway.bridge(&["windows"]);
    let id = |t: &str| {
        w.as_array()
            .unwrap()
            .iter()
            .find(|x| x["title"] == t)
            .unwrap_or_else(|| panic!("{t} not in windows: {w}"))["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| w.to_string())
    };
    (id("left-win"), id("right-win"))
}

#[test]
fn enumeration_and_masked_capture_with_the_overlay_up() {
    require!("sway");
    require!("foot");
    let sway = Sway::start();
    let (left, right) = two_windows(&sway);

    let windows = sway.bridge(&["windows"]);
    let front = sway.bridge(&["frontmost-app"]);
    let under = sway.bridge(&["app-under-point", "--x", "300", "--y", "200"]);
    let (s, _) = shot(&sway, "windows-no-overlay", &[]);
    assert!(
        foot_bg(s.px(320, 400)) && foot_bg(s.px(960, 400)),
        "foot bg"
    );

    let mut ov = Overlay::spawn(&sway, &[]);
    expect_event(&ov.lines, "ready");
    ov.send(SHOW_RIM);
    expect_event(&ov.lines, "shown");
    std::thread::sleep(Duration::from_millis(500));

    // Enumeration ignores the layer surface entirely.
    assert_eq!(sway.bridge(&["windows"]), windows, "windows changed");
    assert_eq!(
        sway.bridge(&["frontmost-app"]),
        front,
        "frontmost-app changed"
    );
    assert_eq!(
        sway.bridge(&["app-under-point", "--x", "300", "--y", "200"]),
        under,
        "app-under-point at the arrow changed"
    );

    // Unmasked: the overlay is in the image (accepted), the windows below too.
    let (s, _) = shot(&sway, "windows-overlay-unmasked", &[]);
    assert_rim_and_arrow(&s, foot_bg);

    // Masked, keep the left window: it keeps its pixels (with the rim and
    // arrow composited over it); the right half is masked, rim included.
    // The output cut, explicitly (sway's default too): the path where a
    // layer surface could cut a kept window.
    let cut = [("WLROOTS_BRIDGE_MASK_CAPTURE", "output")];
    let (s, v) = shot_env(
        &sway,
        "windows-overlay-keep-left",
        &["--keep-window", &left],
        &cut,
    );
    let m = &v["mask"];
    assert_eq!(m["mode"], "keep", "{m}");
    assert_eq!(m["capture"], "output", "{m}");
    assert_eq!(m["kept"][0]["shown"], true, "{m}");
    assert!(
        !m["kept"][0]["coveredBy"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "layer:claude-cu-overlay"),
        "our overlay must not be listed in coveredBy: {m}"
    );
    assert!(
        foot_bg(s.px(320, 400)),
        "kept window hidden: {:?}",
        s.px(320, 400)
    );
    assert!(
        clay(s.px(0, 400)),
        "rim over the kept window: {:?}",
        s.px(0, 400)
    );
    assert!(clay(s.px(312, 212)), "arrow over the kept window");
    assert!(
        masked(s.px(960, 400)),
        "right window not masked: {:?}",
        s.px(960, 400)
    );
    assert!(
        masked(s.px(1279, 400)),
        "rim over a non-kept window must be masked"
    );
    assert!(
        masked(s.px(960, 0)) && masked(s.px(960, 799)),
        "rim edges outside the keep"
    );

    // Keep the right window instead: the arrow (over the left) is masked.
    let (s, v) = shot_env(
        &sway,
        "windows-overlay-keep-right",
        &["--keep-window", &right],
        &cut,
    );
    assert_eq!(v["mask"]["kept"][0]["shown"], true, "{}", v["mask"]);
    assert!(foot_bg(s.px(960, 400)));
    assert!(
        masked(s.px(312, 212)) && masked(s.px(0, 400)),
        "arrow/rim over non-kept"
    );

    // Per-window capture (ext, behind the override on sway; when headless
    // sway advertises it): the kept window comes without the overlay.
    let g = &sway.bridge(&["doctor"])["globals"];
    if !g["image_copy_capture"].is_null() && !g["toplevel_capture_source"].is_null() {
        let (s, v) = shot_env(
            &sway,
            "windows-overlay-keep-left-ext",
            &["--keep-window", &left],
            &[("WLROOTS_BRIDGE_MASK_CAPTURE", "ext")],
        );
        let m = &v["mask"];
        assert_eq!(m["capture"], "toplevel", "{m}");
        assert_eq!(m["kept"][0]["shown"], true, "{m}");
        assert!(foot_bg(s.px(320, 400)), "kept window: {:?}", s.px(320, 400));
        assert!(!clay(s.px(312, 212)), "no arrow in a per-window capture");
        assert!(masked(s.px(960, 400)) && masked(s.px(1279, 400)));
    } else {
        eprintln!("headless sway has no ext toplevel capture: per-window check skipped");
    }

    // --mask-all is all mask colour, overlay or not.
    let (s, _) = shot(&sway, "windows-overlay-mask-all", &["--mask-all"]);
    assert_eq!(s.count(0, 0, 1280, 800, masked), 1280 * 800);

    ov.stdin.take();
    assert_eq!(ov.wait_exit(T), Some(0));
}
