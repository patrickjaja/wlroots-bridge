//! `abort-watch` against REAL sway compositors (skipped when `sway` is not
//! installed). Private instances only: own XDG_RUNTIME_DIR, headless.
//!
//! Headless sway has no physical keyboard, so the tests nest a second sway
//! (wayland backend) inside the headless one. The inner sway gets a real,
//! non-virtual keyboard device (`wayland-keyboard-seat0`) that carries the
//! keys the outer compositor sends to its window: an Escape typed into the
//! OUTER sway (with the bridge's virtual keyboard) arrives in the inner sway
//! the way a physical key would. An Escape typed into the INNER sway with the
//! bridge is the model's synthetic key (`wlr_virtual_keyboard_v1`).
//!
//! The inner sway reads that device with its own default (US) keymap. That
//! works because the bridge sends keys on their US evdev codes (Escape on
//! KEY_ESC, Control on KEY_LEFTCTRL); before that change the tests needed a
//! custom xkb file.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::{BIN, TempDir, Watcher, wait_for};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(10);

fn have_sway() -> bool {
    Command::new("sway")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Without sway the tests are skipped, unless `WLROOTS_BRIDGE_REQUIRE_SWAY=1`
/// (CI): then a missing sway fails them instead of passing vacuously.
macro_rules! require_sway {
    () => {
        if !have_sway() {
            if std::env::var("WLROOTS_BRIDGE_REQUIRE_SWAY").is_ok_and(|v| v == "1") {
                panic!("sway is required (WLROOTS_BRIDGE_REQUIRE_SWAY=1) but not installed");
            }
            eprintln!("sway not installed: skipping");
            return;
        }
    };
}

struct Sway {
    rt: TempDir,
    child: Child,
}

impl Sway {
    fn sock(&self) -> PathBuf {
        self.rt.path().join("sway.sock")
    }

    fn cfg(&self) -> PathBuf {
        self.rt.path().join("sway.cfg")
    }

    fn start(rt: TempDir, backend: &str, wayland_display: Option<&str>, cfg: &str) -> Self {
        std::fs::write(rt.path().join("sway.cfg"), cfg).unwrap();
        let mut cmd = Command::new("sway");
        cmd.arg("-c")
            .arg(rt.path().join("sway.cfg"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", rt.path())
            .env("WLR_BACKENDS", backend)
            .env("WLR_RENDERER", "pixman")
            .env("SWAYSOCK", rt.path().join("sway.sock"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(rt.path().join("sway.log")).unwrap());
        if backend == "headless" {
            cmd.env("WLR_LIBINPUT_NO_DEVICES", "1");
        }
        if let Some(d) = wayland_display {
            cmd.env("WAYLAND_DISPLAY", d);
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

    fn log(&self) -> String {
        std::fs::read_to_string(self.rt.path().join("sway.log")).unwrap_or_default()
    }

    fn run(&self, cmd: &str) -> Value {
        ipc(&self.sock(), 0, cmd)
    }

    /// Type `keys` into this compositor with the bridge's virtual keyboard.
    fn key(&self, keys: &str) {
        let out = Command::new(BIN)
            .args(["key-sequence", "--keys", keys])
            .env_clear()
            .env("XDG_RUNTIME_DIR", self.rt.path())
            .env("WAYLAND_DISPLAY", "wayland-1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    /// Give this compositor's seat a keyboard for a moment without typing
    /// a key that means anything: `hold-key ctrl` presses keycode 9, which
    /// is Control in the inner sway's keymap too (`key-sequence` would put
    /// any key on 13 = Escape there).
    fn plug(&self) {
        let out = Command::new(BIN)
            .args(["hold-key", "--key", "ctrl", "--duration-ms", "50"])
            .env_clear()
            .env("XDG_RUNTIME_DIR", self.rt.path())
            .env("WAYLAND_DISPLAY", "wayland-1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    /// `claude-cu-esc/sway/<instance>`: the holder set of THIS sway.
    fn holder_dir(&self) -> PathBuf {
        let base = self.rt.path().join("claude-cu-esc/sway");
        let dirs: Vec<PathBuf> = std::fs::read_dir(&base)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            dirs.len(),
            1,
            "one instance dir in {}: {dirs:?}",
            base.display()
        );
        dirs[0].clone()
    }

    fn watcher(&self) -> Watcher {
        let rt = self.rt.path().display().to_string();
        let sock = self.sock().display().to_string();
        Watcher::spawn(&[("XDG_RUNTIME_DIR", &rt), ("SWAYSOCK", &sock)])
    }

    /// Binding events of this sway, from a subscription made now.
    fn bindings(&self) -> mpsc::Receiver<String> {
        let mut s = UnixStream::connect(self.sock()).unwrap();
        s.write_all(&frame(2, "[\"binding\"]")).unwrap();
        read_msg(&mut s);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let Some((_, v)) = try_read_msg(&mut s) else {
                    return;
                };
                let c = v
                    .pointer("/binding/command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if tx.send(c).is_err() {
                    return;
                }
            }
        });
        rx
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn frame(ty: u32, body: &str) -> Vec<u8> {
    let mut m = b"i3-ipc".to_vec();
    m.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(body.as_bytes());
    m
}

fn try_read_msg(s: &mut UnixStream) -> Option<(u32, Value)> {
    let mut head = [0u8; 14];
    s.read_exact(&mut head).ok()?;
    let len = u32::from_ne_bytes(head[6..10].try_into().unwrap()) as usize;
    let ty = u32::from_ne_bytes(head[10..14].try_into().unwrap());
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    Some((ty, serde_json::from_slice(&body).unwrap()))
}

fn read_msg(s: &mut UnixStream) -> Value {
    try_read_msg(s).expect("i3-ipc reply").1
}

fn ipc(sock: &Path, ty: u32, payload: &str) -> Value {
    let mut s = UnixStream::connect(sock).unwrap();
    s.write_all(&frame(ty, payload)).unwrap();
    read_msg(&mut s)
}

/// Outer headless sway + inner nested sway with a non-virtual keyboard.
struct Pair {
    inner: Sway,
    outer: Sway,
}

impl Pair {
    fn start(inner_extra: &str) -> Self {
        let outer = Sway::start(
            TempDir::new("o"),
            "headless",
            None,
            "xwayland disable\noutput HEADLESS-1 resolution 800x600\n",
        );
        let irt = TempDir::new("i");
        std::os::unix::fs::symlink(
            outer.rt.path().join("wayland-1"),
            irt.path().join("outer-wl"),
        )
        .unwrap();
        // The inner sway's own (default US) keymap reads the outer keys
        // right: the bridge sends Escape on KEY_ESC and Control on
        // KEY_LEFTCTRL.
        let cfg = format!("xwayland disable\n{inner_extra}\n");
        let inner = Sway::start(irt, "wayland", Some("outer-wl"), &cfg);
        // The inner sway's window must be focused in the outer one.
        assert!(wait_for(T, || {
            ipc(&outer.sock(), 4, "")
                .to_string()
                .contains("\"focused\":true,")
        }));
        Self { inner, outer }
    }

    /// The user presses Escape on the inner sway's physical keyboard.
    fn physical_escape(&self) {
        self.outer.key("escape");
    }

    /// Make the inner keyboard appear (it exists only while the outer seat
    /// has a keyboard) and wait until a holder bound it (any holder: the
    /// first to take the lock binds, the shared record shows it).
    fn plug_keyboard(&self, w: &Watcher) {
        self.outer.plug();
        let dir = self.inner.holder_dir();
        assert!(
            wait_for(T, || dir.join("bound.json").exists()
                && !record_devices(&dir).is_empty()),
            "keyboard not bound; stderr: {}",
            w.stderr_text()
        );
    }

    /// The bridge (the model) presses Escape in the inner sway.
    fn synthetic_escape(&self) {
        self.inner.key("escape");
    }

    /// Whether a bind of ours is live in the inner sway, without touching
    /// it: press Escape on the physical keyboard and watch for our binding
    /// running (use while no watcher should hold a bind).
    fn ours_fires(&self) -> bool {
        let runs = self.inner.bindings();
        self.physical_escape();
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match runs.recv_timeout(left) {
                Ok(c) if c == "nop claude-cu-abort" => return true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        false
    }

    /// The user binding commands a physical Escape runs right now.
    fn runs_for_escape(&self) -> Vec<String> {
        let runs = self.inner.bindings();
        self.physical_escape();
        std::thread::sleep(Duration::from_millis(600));
        runs.try_iter().collect()
    }

    /// Append a line to the inner config and reload.
    fn reload_with(&self, line: &str) {
        let mut cfg = std::fs::read_to_string(self.inner.cfg()).unwrap();
        cfg.push_str(line);
        cfg.push('\n');
        std::fs::write(self.inner.cfg(), cfg).unwrap();
        self.inner.run("reload");
        std::thread::sleep(Duration::from_millis(400));
    }
}

fn record_devices(dir: &Path) -> Vec<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join("bound.json")).unwrap()).unwrap();
    v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap().to_owned())
        .collect()
}

fn holder_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.parse::<u32>().is_ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn physical_escape_aborts_synthetic_does_not_and_eof_cleans_up() {
    require_sway!();
    let p = Pair::start("");
    let mut w = p.inner.watcher();
    assert_eq!(
        w.expect_ready(),
        json!({"event":"ready","mechanism":"sway","syntheticSafe":true})
    );
    p.plug_keyboard(&w);
    let ids = record_devices(&p.inner.holder_dir());
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert!(!ids[0].contains("virtual"), "{ids:?}");

    p.physical_escape();
    assert_eq!(w.expect(T), json!({"event":"abort"}));

    // The model's Escape (virtual keyboard) never aborts.
    let user = p.inner.bindings();
    p.synthetic_escape();
    p.synthetic_escape();
    assert_eq!(w.quiet_for(Duration::from_millis(1200)), None);
    assert!(
        user.try_recv().is_err(),
        "no binding ran for the virtual keyboard"
    );

    p.physical_escape();
    assert_eq!(w.expect(T), json!({"event":"abort"}));

    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    assert!(!p.ours_fires(), "bind removed on EOF");
    assert!(holder_files(&p.inner.holder_dir()).is_empty());
    assert!(!p.inner.holder_dir().join("bound.json").exists());
}

#[test]
fn sigterm_cleans_up() {
    require_sway!();
    let p = Pair::start("");
    let w = p.inner.watcher();
    w.expect_ready();
    p.plug_keyboard(&w);
    let mut w = w;
    w.signal("TERM");
    assert_eq!(w.wait(T).code(), Some(0));
    assert!(!p.ours_fires(), "bind removed on SIGTERM");
}

#[test]
fn refuses_when_user_binds_escape_and_keeps_it() {
    require_sway!();
    let p = Pair::start("bindsym Escape nop user-esc");
    let mut w = p.inner.watcher();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("bindsym Escape nop user-esc"), "{msg}");
    assert_eq!(w.wait(T).code(), Some(2));

    // The user's binding still runs for a physical Escape.
    let runs = p.inner.bindings();
    p.outer.plug();
    p.physical_escape();
    assert_eq!(runs.recv_timeout(T).unwrap(), "nop user-esc");
    assert!(holder_files(&p.inner.holder_dir()).is_empty());
}

#[test]
fn two_holders_one_bind_last_unbinds() {
    require_sway!();
    let p = Pair::start("");
    let mut a = p.inner.watcher();
    a.expect_ready();
    let mut b = p.inner.watcher();
    b.expect_ready();
    assert_eq!(holder_files(&p.inner.holder_dir()).len(), 2);
    p.plug_keyboard(&a);

    // One Escape reaches every armed holder.
    p.physical_escape();
    assert_eq!(a.expect(T), json!({"event":"abort"}));
    assert_eq!(b.expect(T), json!({"event":"abort"}));

    // First one out leaves the bind for the other.
    a.signal("TERM");
    assert_eq!(a.wait(T).code(), Some(0));
    std::thread::sleep(Duration::from_millis(1100)); // debounce window
    p.physical_escape();
    assert_eq!(b.expect(T), json!({"event":"abort"}));

    // Last one out removes it.
    b.close_stdin();
    assert_eq!(b.wait(T).code(), Some(0));
    assert!(!p.ours_fires());
}

#[test]
fn sigkilled_holder_is_cleared_by_the_next_first_holder() {
    require_sway!();
    let p = Pair::start("");
    let mut a = p.inner.watcher();
    a.expect_ready();
    p.plug_keyboard(&a);
    a.signal("KILL");
    a.wait(T);

    // The crashed holder's bind is still live.
    let runs = p.inner.bindings();
    p.physical_escape();
    assert_eq!(runs.recv_timeout(T).unwrap(), "nop claude-cu-abort");

    let mut b = p.inner.watcher();
    b.expect_ready();
    assert!(
        b.stderr_text()
            .contains("clearing binds left by a crashed holder"),
        "{}",
        b.stderr_text()
    );
    p.plug_keyboard(&b);
    p.physical_escape();
    assert_eq!(b.expect(T), json!({"event":"abort"}));
    b.close_stdin();
    assert_eq!(b.wait(T).code(), Some(0));
    assert!(!p.ours_fires());
}

#[test]
fn reload_rebinds_and_a_new_user_binding_stops_the_watcher() {
    require_sway!();
    let p = Pair::start("");
    let mut w = p.inner.watcher();
    w.expect_ready();
    p.plug_keyboard(&w);

    p.inner.run("reload");
    assert!(wait_for(T, || w
        .stderr_text()
        .contains("re-bound after sway reload")));
    p.physical_escape();
    assert_eq!(w.expect(T), json!({"event":"abort"}));

    // The user adds an Escape binding and reloads: we step aside.
    let mut cfg = std::fs::read_to_string(p.inner.cfg()).unwrap();
    cfg.push_str("bindsym Escape nop user-esc\n");
    std::fs::write(p.inner.cfg(), cfg).unwrap();
    p.inner.run("reload");
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(w.wait(T).code(), Some(1));
    assert_eq!(p.runs_for_escape(), vec!["nop user-esc"]);
}

#[test]
fn binds_into_default_mode_while_the_user_is_in_another_mode() {
    require_sway!();
    let p = Pair::start("mode \"resize\" {\n  bindsym Escape mode default\n}");
    p.inner.run("mode resize");
    let mut w = p.inner.watcher();
    w.expect_ready();
    p.plug_keyboard(&w);
    // Arming did not change the mode.
    assert_eq!(ipc(&p.inner.sock(), 12, "")["name"], "resize");

    // An Escape in `resize` runs the user's `mode default`. sway 1.12 then
    // evaluates the same press in `default`, where our bind runs too; that
    // run follows a mode change and is dropped (no abort).
    let runs = p.inner.bindings();
    p.physical_escape();
    assert_eq!(runs.recv_timeout(T).unwrap(), "mode default");
    assert_eq!(ipc(&p.inner.sock(), 12, "")["name"], "default");
    assert_eq!(
        w.quiet_for(Duration::from_millis(1200)),
        None,
        "no abort when leaving resize"
    );

    // In the default mode, Escape aborts.
    p.physical_escape();
    assert_eq!(w.expect(T), json!({"event":"abort"}));
    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
}

// ---------------------------------------------------------- review fixes

/// R1: a user binding with OUR signature (device + `--inhibited` + Escape)
/// added by a reload must survive our cleanup.
#[test]
fn reload_with_a_same_signature_user_binding_keeps_it() {
    require_sway!();
    let p = Pair::start("");
    let mut w = p.inner.watcher();
    w.expect_ready();
    p.plug_keyboard(&w);
    let id = record_devices(&p.inner.holder_dir())[0].clone();
    p.reload_with(&format!(
        "bindsym --input-device={id} --inhibited Escape nop user-dev"
    ));
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(w.wait(T).code(), Some(1));
    assert_eq!(p.runs_for_escape(), vec!["nop user-dev"]);
}

/// R1: stale-bind cleanup at the next arm must not remove a same-signature
/// user binding either.
#[test]
fn stale_cleanup_skips_a_same_signature_user_binding() {
    require_sway!();
    let p = Pair::start("");
    let mut a = p.inner.watcher();
    a.expect_ready();
    p.plug_keyboard(&a);
    let id = record_devices(&p.inner.holder_dir())[0].clone();
    a.signal("KILL");
    a.wait(T);
    // The user binds the same signature and reloads (which drops our stale bind).
    p.reload_with(&format!(
        "bindsym --input-device={id} --inhibited Escape nop user-dev"
    ));
    let mut b = p.inner.watcher();
    let e = b.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(b.wait(T).code(), Some(2));
    assert_eq!(p.runs_for_escape(), vec!["nop user-dev"]);
}

/// R2: two sway instances of one user (one runtime dir) keep separate
/// holder sets.
#[test]
fn two_sway_instances_do_not_share_holders() {
    require_sway!();
    let p = Pair::start("");
    let other = Sway::start(TempDir::new("b"), "headless", None, "xwayland disable\n");
    let mut a = p.inner.watcher();
    a.expect_ready();
    p.plug_keyboard(&a);
    // Profile 2 on the other sway, same XDG_RUNTIME_DIR.
    let rt = p.inner.rt.path().display().to_string();
    let sock = other.sock().display().to_string();
    let mut b = Watcher::spawn(&[("XDG_RUNTIME_DIR", &rt), ("SWAYSOCK", &sock)]);
    b.expect_ready();
    let sets: Vec<_> = std::fs::read_dir(p.inner.rt.path().join("claude-cu-esc/sway"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(sets.len(), 2, "one holder set per sway instance");
    a.signal("TERM");
    assert_eq!(a.wait(T).code(), Some(0));
    assert!(
        !p.ours_fires(),
        "A's last holder unbound A while B still runs"
    );
    b.close_stdin();
    assert_eq!(b.wait(T).code(), Some(0));
}

/// R3: `abort-watch` with stdin already at EOF heals a crashed holder's bind.
#[test]
fn heal_with_closed_stdin_clears_a_crashed_holders_bind() {
    require_sway!();
    let p = Pair::start("");
    let mut a = p.inner.watcher();
    a.expect_ready();
    p.plug_keyboard(&a);
    a.signal("KILL");
    a.wait(T);
    assert!(p.ours_fires(), "the crashed holder left its bind");

    let out = Command::new(BIN)
        .arg("doctor")
        .env_clear()
        .env("XDG_RUNTIME_DIR", p.inner.rt.path())
        .env("SWAYSOCK", p.inner.sock())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["abortWatch"]["stale"], true, "{}", v["abortWatch"]);

    let heal = Command::new(BIN)
        .arg("abort-watch")
        .env_clear()
        .env("XDG_RUNTIME_DIR", p.inner.rt.path())
        .env("SWAYSOCK", p.inner.sock())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        heal.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&heal.stderr)
    );
    assert!(!p.ours_fires(), "stale bind cleared");
    let dir = p.inner.holder_dir();
    assert!(holder_files(&dir).is_empty());
    assert!(!dir.join("bound.json").exists());
}

#[test]
fn doctor_reports_sway_abort_watch() {
    require_sway!();
    let s = Sway::start(
        TempDir::new("d"),
        "headless",
        None,
        "xwayland disable\nbindsym Escape nop x\n",
    );
    let out = Command::new(BIN)
        .arg("doctor")
        .env_clear()
        .env("XDG_RUNTIME_DIR", s.rt.path())
        .env("SWAYSOCK", s.sock())
        .env("WAYLAND_DISPLAY", "wayland-1")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["abortWatch"]["mechanism"], "sway");
    assert_eq!(v["abortWatch"]["available"], false);
    assert!(
        v["abortWatch"]["reason"]
            .as_str()
            .unwrap()
            .contains("bindsym Escape nop x")
    );
    let s2 = Sway::start(TempDir::new("d"), "headless", None, "xwayland disable\n");
    let out = Command::new(BIN)
        .arg("doctor")
        .env_clear()
        .env("XDG_RUNTIME_DIR", s2.rt.path())
        .env("SWAYSOCK", s2.sock())
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["abortWatch"],
        json!({"mechanism":"sway","available":true,"reason":null,
               "stale":false,"skippedKeyboards":[],"consumes":true})
    );
}

// ------------------------------------------- virtual keyboard key codes

/// Run bridge input commands against xkbcommon's own Wayland test client in
/// a private headless sway; returns each key-down as (xkb keycode, keysym,
/// unicode). `None` when the client is not installed. The client exits on
/// Escape (flushing its output), so Escape is sent last.
fn keys_seen(cmds: &[&[&str]]) -> Option<Vec<(u32, String, String)>> {
    let client = Path::new("/usr/lib/xkbcommon/xkbcli-interactive-wayland");
    if !client.exists() {
        eprintln!("xkbcli-interactive-wayland not installed: skipping");
        return None;
    }
    let s = Sway::start(
        TempDir::new("k"),
        "headless",
        None,
        "xwayland disable\noutput HEADLESS-1 resolution 800x600\n",
    );
    let out = s.rt.path().join("keys.out");
    let mut c = Command::new(client)
        .env_clear()
        .env("XDG_RUNTIME_DIR", s.rt.path())
        .env("WAYLAND_DISPLAY", "wayland-1")
        .stdout(std::fs::File::create(&out).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let escape: &[&str] = &["key-sequence", "--keys", "escape"];
    for args in cmds.iter().chain(std::iter::once(&escape)) {
        let o = Command::new(BIN)
            .args(*args)
            .env_clear()
            .env("XDG_RUNTIME_DIR", s.rt.path())
            .env("WAYLAND_DISPLAY", "wayland-1")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        std::thread::sleep(Duration::from_millis(150));
    }
    let _ = wait_for(T, || c.try_wait().unwrap().is_some());
    let _ = c.kill();
    let text = std::fs::read_to_string(&out).unwrap();
    // `key down [ K<xkb> ] keysyms [ <name> ] unicode [ <u> ] ...`
    let field = |l: &str, tag: &str| -> String {
        let rest = l.split(tag).nth(1).unwrap_or("");
        rest.split(']').next().unwrap_or("").trim().to_owned()
    };
    Some(
        text.lines()
            .filter(|l| l.contains("key down"))
            .map(|l| {
                let kc = field(l, "[ K").parse().unwrap();
                (kc, field(l, "keysyms ["), field(l, "unicode ["))
            })
            .collect(),
    )
}

/// Keys the bridge injects sit at their US evdev positions (Escape on
/// KEY_ESC, Control on KEY_LEFTCTRL, ...) and still carry exactly the
/// requested keysyms. Hyprland matches a virtual keyboard's key by its code
/// in the user's layout, so a wrong code fires the wrong binds there.
#[test]
fn injected_keys_sit_at_standard_codes() {
    require_sway!();
    let Some(got) = keys_seen(&[
        &["key-sequence", "--keys", "ctrl+a"],
        &["type", "--text", "Hi!é好"],
    ]) else {
        return;
    };
    let got: Vec<(u32, &str)> = got.iter().map(|(k, s, _)| (*k, s.as_str())).collect();
    let want: Vec<(u32, &str)> = vec![
        (37, "Control_L"), // evdev 29
        (38, "a"),         // evdev 30, with Control held: ^A
        (43, "H"),         // US position of h/H
        (31, "i"),
        (10, "exclam"), // US position of 1/!
        (97, "eacute"), // no US position: first unmapped code
        (103, "U597D"),
        (9, "Escape"), // evdev 1 = KEY_ESC
    ];
    assert_eq!(got, want);
}

/// A combo that holds Shift gets the shifted symbol, as on a real keyboard
/// (it used to be the unshifted one: `shift+a` gave `a`), while `type` and
/// Shift-less keys still deliver exactly the requested glyph.
#[test]
fn shift_combos_deliver_shifted_symbols() {
    require_sway!();
    let Some(got) = keys_seen(&[
        &["key-sequence", "--keys", "shift+a"],
        &["key-sequence", "--keys", "shift+1"],
        &["key-sequence", "--keys", "ctrl+shift+a"],
        &["key-sequence", "--keys", "shift+tab"],
        &["key-sequence", "--keys", "shift+a", "--repeat", "2"],
        &[
            "hold-key",
            "--key",
            "shift",
            "--key",
            "a",
            "--duration-ms",
            "50",
        ],
        &["key-sequence", "--keys", "a"],
        &["key-sequence", "--keys", "A"],
        &["type", "--text", "aB!é好~"],
    ]) else {
        return;
    };
    // Drop the modifier presses; keep (keysym, unicode) of the keys.
    let keys: Vec<(&str, &str)> = got
        .iter()
        .filter(|(kc, _, _)| ![37, 50].contains(kc))
        .map(|(_, s, u)| (s.as_str(), u.as_str()))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("A", "A"),
            ("exclam", "!"),
            ("A", "U+0001"), // ctrl+shift+a: Control + A
            ("ISO_Left_Tab", ""),
            ("A", "A"),
            ("A", "A"),
            ("A", "A"), // hold-key shift + a
            ("a", "a"),
            ("A", "A"),
            ("a", "a"),
            ("B", "B"),
            ("exclam", "!"),
            ("eacute", "é"),
            ("U597D", "好"),
            ("asciitilde", "~"),
            ("Escape", "U+001B"),
        ]
    );
}
