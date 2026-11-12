//! `type` against a REAL headless sway with a German layout (skipped when
//! `sway` is not installed, unless `WLROOTS_BRIDGE_REQUIRE_SWAY=1`). Private
//! instance only: own short XDG_RUNTIME_DIR, `WLR_BACKENDS=headless`, no input
//! devices, pixman renderer.
//!
//! Two sinks:
//!
//! - `foot` running `cat` on a raw tty: the exact text that arrived, across
//!   keymap pages (needs `foot`; required with `WLROOTS_BRIDGE_REQUIRE_SWAY=1`).
//! - a minimal Wayland client in this file that records the key codes it gets.
//!   Chromium (so every Electron app) decides from the code alone whether a key
//!   is a character key: a character on a code it has no name for never
//!   arrives, one on Backspace deletes. So every code `type` sends must be a
//!   character key (or the character's own named key).

mod common;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{BIN, TempDir, wait_for};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const T: Duration = Duration::from_secs(10);

/// 58 distinct characters: more than one keymap page holds. Upper case next to
/// its own lower case, US-shifted symbols, German letters, dead-key symbols,
/// CJK and a non-BMP emoji.
const TORTURE: &str = "abc ABC gnmebc GNMEBC + - _ / \\ @ { } [ ] | ~ # ' \" ; : < > ! ? = * % & ( ) $ € ß ä ö ü Ä Ö Ü ^ ´ ` 😀 漢字 0189";

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

struct Sway {
    rt: TempDir,
    child: Child,
}

impl Sway {
    fn start(layout: &str) -> Self {
        let rt = TempDir::new("ty");
        let cfg = rt.path().join("sway.cfg");
        std::fs::write(
            &cfg,
            format!(
                "xwayland disable\noutput HEADLESS-1 resolution 1280x800 position 0 0\n\
                 default_border none\nfocus_follows_mouse no\ninput * xkb_layout {layout}\n"
            ),
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
        // foot needs a home for fontconfig's cache.
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

    fn env(&self, cmd: &mut Command) {
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", self.rt.path())
            .env("WAYLAND_DISPLAY", "wayland-1")
            .env("SWAYSOCK", self.sock());
    }

    fn swaymsg(&self, args: &[&str]) {
        let mut cmd = Command::new("swaymsg");
        self.env(&mut cmd);
        let out = cmd.args(args).output().unwrap();
        assert!(out.status.success(), "swaymsg {args:?}: {out:?}");
    }

    fn tree(&self) -> String {
        let mut cmd = Command::new("swaymsg");
        self.env(&mut cmd);
        let out = cmd.args(["-t", "get_tree"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn type_text(&self, text: &str, delay_ms: u32) {
        let mut cmd = Command::new(BIN);
        self.env(&mut cmd);
        let out = cmd
            .args(["type", "--text", text, "--delay-ms", &delay_ms.to_string()])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "type failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["charCount"], text.chars().count(), "{v}");
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Type `text` into a fresh foot whose tty is raw and copied to a file; the
/// file's contents once they stop changing.
fn type_into_foot(sway: &Sway, text: &str, delay_ms: u32) -> String {
    let out = sway.rt.path().join(format!("foot-{delay_ms}.out"));
    let _ = std::fs::remove_file(&out);
    sway.swaymsg(&[&format!(
        "exec foot -c /dev/null -a typesink sh -c \"stty raw -echo; exec cat > {}\"",
        out.display()
    )]);
    let up = wait_for(T, || sway.tree().contains("\"typesink\"") && out.exists());
    assert!(up, "foot did not start: {}", sway.log());
    std::thread::sleep(Duration::from_millis(300));
    sway.type_text(text, delay_ms);
    let want = text.len();
    wait_for(T, || {
        std::fs::metadata(&out).is_ok_and(|m| m.len() as usize >= want)
    });
    std::thread::sleep(Duration::from_millis(200));
    sway.swaymsg(&["[app_id=\"typesink\"] kill"]);
    String::from_utf8_lossy(&std::fs::read(&out).unwrap()).into_owned()
}

#[test]
fn type_arrives_intact_in_foot_with_german_layout() {
    require!("sway");
    require!("foot");
    let sway = Sway::start("de");
    for delay in [12, 0] {
        let got = type_into_foot(&sway, TORTURE, delay);
        assert_eq!(got, TORTURE, "delay {delay} ms");
    }
}

// ------------------------------------------------- key-code recording client

#[derive(Default)]
struct Rec {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    configured: bool,
    focused: bool,
    keymaps: usize,
    pressed: Vec<u32>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Rec {
    fn event(
        s: &mut Self,
        reg: &wl_registry::WlRegistry,
        ev: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = ev
        {
            match interface.as_str() {
                "wl_compositor" => s.compositor = Some(reg.bind(name, 4, qh, ())),
                "wl_shm" => s.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => s.wm = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" => s.seat = Some(reg.bind(name, 5, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Rec {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        ev: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Rec {
    fn event(
        s: &mut Self,
        xs: &xdg_surface::XdgSurface,
        ev: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            let surface = s.surface.as_ref().unwrap();
            surface.attach(s.buffer.as_ref(), 0, 0);
            surface.damage(0, 0, 64, 64);
            surface.commit();
            s.configured = true;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Rec {
    fn event(
        s: &mut Self,
        seat: &wl_seat::WlSeat,
        ev: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = ev
            && caps.contains(wl_seat::Capability::Keyboard)
            && s.keyboard.is_none()
        {
            s.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Rec {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            wl_keyboard::Event::Keymap { .. } => s.keymaps += 1,
            wl_keyboard::Event::Enter { .. } => s.focused = true,
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(wl_keyboard::KeyState::Pressed),
                ..
            } => s.pressed.push(key + 8),
            _ => {}
        }
    }
}

delegate_noop!(Rec: ignore wl_compositor::WlCompositor);
delegate_noop!(Rec: ignore wl_shm::WlShm);
delegate_noop!(Rec: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Rec: ignore wl_buffer::WlBuffer);
delegate_noop!(Rec: ignore wl_surface::WlSurface);
delegate_noop!(Rec: ignore xdg_toplevel::XdgToplevel);

/// A 64x64 shm buffer (contents irrelevant).
fn shm_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<Rec>) -> wl_buffer::WlBuffer {
    let size = 64 * 64 * 4;
    // SAFETY: memfd_create with a valid NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"typesink".as_ptr(), 0) };
    assert!(fd >= 0);
    // SAFETY: fresh owned fd.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: valid fd, size in range.
    assert_eq!(unsafe { libc::ftruncate(fd.as_fd().as_raw_fd(), size) }, 0);
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buf = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    buf
}

/// Character keys of the US layout (alphanumeric block) plus IntlBackslash,
/// IntlRo and IntlYen: what `type` may put any character on.
fn character_key(kc: u32) -> bool {
    (10..=21).contains(&kc)
        || (24..=35).contains(&kc)
        || (38..=49).contains(&kc)
        || (51..=61).contains(&kc)
        || [94, 97, 132].contains(&kc)
}

#[test]
fn type_sends_only_character_key_codes() {
    require!("sway");
    let sway = Sway::start("de");
    let conn = connect(&sway);
    let mut queue = conn.new_event_queue::<Rec>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut rec = Rec::default();
    queue.roundtrip(&mut rec).unwrap();
    queue.roundtrip(&mut rec).unwrap();
    let surface = rec.compositor.as_ref().unwrap().create_surface(&qh, ());
    rec.buffer = Some(shm_buffer(rec.shm.as_ref().unwrap(), &qh));
    let xs = rec.wm.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let top = xs.get_toplevel(&qh, ());
    top.set_app_id("codesink".into());
    surface.commit();
    rec.surface = Some(surface);

    // The virtual keyboard appears with the bridge: the keyboard focus may
    // only arrive then, so keep dispatching while typing.
    let text = TORTURE.replace("😀", "").replace('\n', "");
    let typer = {
        let mut cmd = Command::new(BIN);
        sway.env(&mut cmd);
        let ok = wait_until(&mut queue, &mut rec, |r| r.configured);
        assert!(ok, "code sink never configured: {}", sway.log());
        std::thread::sleep(Duration::from_millis(200));
        cmd.args(["type", "--text", &text, "--delay-ms", "0"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    };
    let n = text.chars().count();
    let ok = wait_until(&mut queue, &mut rec, |r| r.pressed.len() >= n);
    let mut typer = typer;
    assert!(typer.wait().unwrap().success());
    assert!(ok, "{} of {n} keys arrived", rec.pressed.len());
    assert!(rec.focused);
    let bad: Vec<String> = text
        .chars()
        .zip(&rec.pressed)
        // Space keeps its own key; everything else a character key.
        .filter(|(ch, kc)| !(character_key(**kc) || (*ch == ' ' && **kc == 65)))
        .map(|(ch, kc)| format!("{ch:?} on {kc}"))
        .collect();
    assert!(bad.is_empty(), "not on a character key: {bad:?}");
    // Two pages: the text holds more distinct characters than one keymap.
    assert!(rec.keymaps >= 2, "{} keymaps", rec.keymaps);
}

fn wait_until(
    queue: &mut wayland_client::EventQueue<Rec>,
    rec: &mut Rec,
    f: impl Fn(&Rec) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + T;
    while std::time::Instant::now() < deadline {
        if f(rec) {
            return true;
        }
        queue.roundtrip(rec).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    f(rec)
}

/// Connect to this sway's socket.
fn connect(sway: &Sway) -> Connection {
    let stream = std::os::unix::net::UnixStream::connect(sway.rt.path().join("wayland-1")).unwrap();
    Connection::from_socket(stream).unwrap()
}
