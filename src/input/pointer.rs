//! Pointer synthesis over `zwlr_virtual_pointer_manager_v1`.
//!
//! Each one-shot command creates a virtual pointer, emits its events, sends a
//! `frame`, roundtrips so the compositor processes them, then exits. Absolute
//! motion uses `motion_absolute(time, x, y, x_extent, y_extent)`. wlroots'
//! `wlr_cursor` maps that normalized position onto the output layout's
//! *bounding box*, including a negative origin (`lx = box.x + x / x_extent *
//! box.w`), so we send the global logical coordinate relative to the layout's
//! top-left (`x - min_x`) with the box size as extent. A layout whose outputs
//! sit left of / above (0,0), or are offset vertically, then maps exactly (the
//! JS sends global logical coords). Buttons use Linux evdev codes; scroll uses `axis`.
//!
//! ## Held-button design (`left-mouse-down` / `left-mouse-up`)
//!
//! A virtual pointer's held buttons are released the moment the client
//! disconnects, so a one-shot process cannot leave a button held. We therefore
//! implement `left-mouse-down` by **daemonizing a holder process**: it forks,
//! detaches, presses button 1 + frame, writes its PID to a pidfile in
//! `$XDG_RUNTIME_DIR`, and then blocks (keeping the Wayland connection - and the
//! held button - alive) until `left-mouse-up` signals it. `left-mouse-up` reads
//! the pidfile and sends SIGTERM; the holder releases the button (its own
//! disconnect also releases it as a backstop) and exits. See DESIGN.md.
//!
//! ## Scroll sign convention
//!
//! Positive `dy` scrolls down, positive `dx` scrolls right - matching the
//! x11-bridge / KDE axis convention (`cu_linux_executor.js` xdotool path and the
//! RemoteDesktop portal both use positive-down / positive-right). ~15 units per
//! wheel notch.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wayland_client::QueueHandle;
use wayland_client::protocol::{wl_pointer, wl_seat};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

use crate::conn::Conn;
use crate::input::signals::{SIGINT, SIGTERM, install_handler};
use crate::output::{ButtonStateResult, DragActionResult, PointerActionResult};
use crate::screens;

/// Linux evdev button codes (see linux/input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// wl_pointer axis values.
const AXIS_VERTICAL: u32 = 0; // wl_pointer.axis vertical_scroll
const AXIS_HORIZONTAL: u32 = 1; // wl_pointer.axis horizontal_scroll

/// Scroll magnitude per wheel notch, matching the KDE/x11-bridge convention.
const SCROLL_STEP: f64 = 15.0;
/// Number of interpolation steps for a drag (mirrors x11-bridge).
const DRAG_STEPS: u32 = 20;
/// Delay between drag interpolation steps.
const DRAG_STEP_MS: u64 = 8;

/// A resolved pointer button.
#[derive(Debug, Clone, Copy)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    /// Parse the CLI `--button` string (`left`/`right`/`middle`).
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            other => bail!("unsupported mouse button: {other}"),
        }
    }

    fn evdev(self) -> u32 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
            Self::Middle => BTN_MIDDLE,
        }
    }
}

/// The virtual pointer manager has no client-visible events.
struct PointerState;
wayland_client::delegate_noop!(PointerState: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(PointerState: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

/// The logical bounding box of the output layout: the space wlroots maps
/// `motion_absolute` onto. Its origin can be negative (an output left of or
/// above (0,0)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutBox {
    min_x: i64,
    min_y: i64,
    max_x: i64,
    max_y: i64,
}

impl LayoutBox {
    /// The union of `(x, y, w, h)` output rectangles (logical layout).
    fn from_rects(rects: impl IntoIterator<Item = (i32, i32, i32, i32)>) -> Option<Self> {
        let mut bbox: Option<Self> = None;
        for (x, y, w, h) in rects {
            let (x, y) = (i64::from(x), i64::from(y));
            let (r, b) = (x + i64::from(w.max(0)), y + i64::from(h.max(0)));
            bbox = Some(match bbox {
                None => Self {
                    min_x: x,
                    min_y: y,
                    max_x: r,
                    max_y: b,
                },
                Some(o) => Self {
                    min_x: o.min_x.min(x),
                    min_y: o.min_y.min(y),
                    max_x: o.max_x.max(r),
                    max_y: o.max_y.max(b),
                },
            });
        }
        bbox
    }

    /// `(x_extent, y_extent)`: the box size, at least 1.
    fn extents(&self) -> (u32, u32) {
        let w = (self.max_x - self.min_x).clamp(1, i64::from(u32::MAX));
        let h = (self.max_y - self.min_y).clamp(1, i64::from(u32::MAX));
        (w as u32, h as u32)
    }

    /// Map a global logical coordinate to the box-relative position sent with
    /// `motion_absolute`, clamped to `[0, extent - 1]` (the last pixel of the
    /// layout; `extent` itself would be one past the right/bottom edge).
    fn map_point(&self, x: i32, y: i32) -> (u32, u32) {
        let (ew, eh) = self.extents();
        let px = (i64::from(x) - self.min_x).clamp(0, i64::from(ew) - 1);
        let py = (i64::from(y) - self.min_y).clamp(0, i64::from(eh) - 1);
        (px as u32, py as u32)
    }
}

/// The logical bounding box of all outputs.
fn layout_box(conn: &Conn) -> Result<LayoutBox> {
    let entries = screens::enumerate(conn)?;
    let rects = entries.iter().map(|e| {
        let g = screens::OutputGeometry::from_entry(e);
        let s = screens::to_screen(&g, false);
        (
            s.geometry.x,
            s.geometry.y,
            s.geometry.width,
            s.geometry.height,
        )
    });
    LayoutBox::from_rects(rects)
        .ok_or_else(|| anyhow::anyhow!("no outputs to derive a coordinate space from"))
}

/// A bound virtual pointer plus the extents its absolute motion maps into.
struct Pointer {
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    layout: LayoutBox,
}

impl Pointer {
    fn create(conn: &Conn, qh: &QueueHandle<PointerState>) -> Result<Self> {
        let layout = layout_box(conn)?;
        let manager = conn.bind_virtual_pointer_manager(qh)?;
        let had_pointer = conn.seat_has(wl_seat::Capability::Pointer);
        // No seat hint: the compositor assigns a default seat.
        let pointer = manager.create_virtual_pointer(None, qh, ());
        conn.settle_new_device(had_pointer)?;
        Ok(Self { pointer, layout })
    }

    /// Warp to a global logical coordinate. Every absolute positioning (move,
    /// click, scroll, drag) goes through here.
    fn motion_absolute(&self, x: i32, y: i32) {
        let (ew, eh) = self.layout.extents();
        let (px, py) = self.layout.map_point(x, y);
        self.pointer.motion_absolute(now_ms(), px, py, ew, eh);
    }

    fn button(&self, evdev: u32, pressed: bool) {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer.button(now_ms(), evdev, state);
    }

    fn axis(&self, axis: u32, value: f64) {
        let axis = if axis == AXIS_HORIZONTAL {
            wl_pointer::Axis::HorizontalScroll
        } else {
            wl_pointer::Axis::VerticalScroll
        };
        self.pointer.axis(now_ms(), axis, value);
    }

    fn frame(&self) {
        self.pointer.frame();
    }
}

/// A millisecond timestamp for the event `time` fields.
fn now_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

pub fn move_pointer(conn: &Conn, x: i32, y: i32) -> Result<PointerActionResult> {
    let mut queue = conn.conn.new_event_queue::<PointerState>();
    let qh = queue.handle();
    let ptr = Pointer::create(conn, &qh)?;

    ptr.motion_absolute(x, y);
    ptr.frame();
    queue
        .roundtrip(&mut PointerState)
        .context("pointer move roundtrip")?;

    Ok(PointerActionResult {
        action: "move".to_owned(),
        x,
        y,
        raised: None,
    })
}

pub fn click(
    conn: &Conn,
    x: i32,
    y: i32,
    button: Button,
    count: u32,
    modifiers: &[String],
) -> Result<PointerActionResult> {
    let repeat = count.max(1);

    // Modifiers are held on the keyboard while clicking (Ctrl+click etc.). We
    // hold them via a virtual keyboard for the duration of the click burst.
    let held_mods = if modifiers.is_empty() {
        None
    } else {
        Some(crate::input::keyboard::ModifierHold::press(
            conn, modifiers,
        )?)
    };

    let mut queue = conn.conn.new_event_queue::<PointerState>();
    let qh = queue.handle();
    let ptr = Pointer::create(conn, &qh)?;

    ptr.motion_absolute(x, y);
    ptr.frame();

    let evdev = button.evdev();
    for i in 0..repeat {
        if i > 0 {
            queue.flush().ok();
            let gap = Duration::from_millis(50);
            let stopped = match &held_mods {
                Some(held) => held.sleep(gap),
                None => {
                    thread::sleep(gap);
                    false
                }
            };
            if stopped {
                break; // stop signal: release the modifiers below
            }
        }
        ptr.button(evdev, true);
        ptr.frame();
        ptr.button(evdev, false);
        ptr.frame();
    }
    queue
        .roundtrip(&mut PointerState)
        .context("pointer click roundtrip")?;

    if let Some(held) = held_mods {
        held.release(); // modifier keys up, mask 0, roundtrip; exits on a stop signal
    }

    Ok(PointerActionResult {
        action: "click".to_owned(),
        x,
        y,
        raised: None,
    })
}

pub fn scroll(conn: &Conn, x: i32, y: i32, dx: f64, dy: f64) -> Result<PointerActionResult> {
    let mut queue = conn.conn.new_event_queue::<PointerState>();
    let qh = queue.handle();
    let ptr = Pointer::create(conn, &qh)?;

    ptr.motion_absolute(x, y);
    ptr.frame();

    // Positive dy = down, positive dx = right. One `axis` per requested notch,
    // SCROLL_STEP units each.
    let v_ticks = dy.round().abs() as u32;
    for _ in 0..v_ticks {
        let value = if dy > 0.0 { SCROLL_STEP } else { -SCROLL_STEP };
        ptr.axis(AXIS_VERTICAL, value);
        ptr.frame();
    }
    let h_ticks = dx.round().abs() as u32;
    for _ in 0..h_ticks {
        let value = if dx > 0.0 { SCROLL_STEP } else { -SCROLL_STEP };
        ptr.axis(AXIS_HORIZONTAL, value);
        ptr.frame();
    }

    queue
        .roundtrip(&mut PointerState)
        .context("pointer scroll roundtrip")?;

    Ok(PointerActionResult {
        action: "scroll".to_owned(),
        x,
        y,
        raised: None,
    })
}

pub fn drag(
    conn: &Conn,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
) -> Result<DragActionResult> {
    let mut queue = conn.conn.new_event_queue::<PointerState>();
    let qh = queue.handle();
    let ptr = Pointer::create(conn, &qh)?;

    ptr.motion_absolute(from_x, from_y);
    ptr.frame();
    ptr.button(BTN_LEFT, true);
    ptr.frame();
    queue.flush().ok();
    thread::sleep(Duration::from_millis(DRAG_STEP_MS));

    for step in 1..=DRAG_STEPS {
        let t = f64::from(step) / f64::from(DRAG_STEPS);
        let x = from_x + ((to_x - from_x) as f64 * t).round() as i32;
        let y = from_y + ((to_y - from_y) as f64 * t).round() as i32;
        ptr.motion_absolute(x, y);
        ptr.frame();
        queue.flush().ok();
        thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    }

    ptr.motion_absolute(to_x, to_y);
    ptr.frame();
    queue.flush().ok();
    thread::sleep(Duration::from_millis(DRAG_STEP_MS));
    ptr.button(BTN_LEFT, false);
    ptr.frame();
    queue
        .roundtrip(&mut PointerState)
        .context("pointer drag roundtrip")?;

    Ok(DragActionResult {
        action: "drag".to_owned(),
        from_x,
        from_y,
        to_x,
        to_y,
    })
}

/// Path of the holder pidfile in `$XDG_RUNTIME_DIR` (profile-suffixed so
/// per-profile Desktop instances don't collide - see claude-desktop-extra's
/// profile system). Falls back to `/tmp`.
fn holder_pidfile() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let profile = std::env::var("CLAUDE_PROFILE").unwrap_or_default();
    let suffix = if profile.is_empty() {
        String::new()
    } else {
        format!("-{profile}")
    };
    std::path::PathBuf::from(format!("{dir}/wlroots-bridge-lmb{suffix}.pid"))
}

/// Press-and-hold the left button by daemonizing a holder process.
///
/// The parent forks; the child detaches (setsid), presses button 1 + frame,
/// writes its PID, and blocks on a signal-driven loop keeping the Wayland
/// connection alive. The parent returns immediately with `is_held:true`.
pub fn left_mouse_down(_conn: &Conn) -> Result<ButtonStateResult> {
    // Note: we do NOT use the passed connection - Wayland proxies are not
    // fork-safe, so the forked holder child opens its own fresh connection.
    let pidfile = holder_pidfile();

    // If a stale holder is recorded, release it first (idempotent down).
    if let Some(pid) = read_pid(&pidfile) {
        let _ = signal_pid(pid, SIGTERM);
        let _ = std::fs::remove_file(&pidfile);
    }

    // Fork a detached holder. We fork BEFORE touching Wayland in the child so
    // the child owns its own connection (Wayland proxies are not fork-safe).
    // SAFETY: fork in a single-threaded one-shot process; the child re-execs no
    // Rust destructors that matter and exits via _exit-equivalent std::process.
    let pid = unsafe { fork() };
    if pid < 0 {
        bail!("fork failed for left-mouse-down holder");
    }
    if pid > 0 {
        // Parent: wait briefly for the child to record its pid so `is_held` is
        // truthful, then return.
        for _ in 0..50 {
            if read_pid(&pidfile).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        return Ok(ButtonStateResult {
            action: "left-mouse-down".to_owned(),
            button: "left".to_owned(),
            is_held: true,
        });
    }

    // --- Child (holder) ---
    // Detach from the controlling terminal / parent process group.
    // SAFETY: setsid on a fresh forked child.
    unsafe {
        setsid();
    }
    // The child gets its own connection + pointer.
    if let Err(e) = run_holder(&pidfile) {
        eprintln!("[wlroots-bridge] holder failed: {e:#}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// The holder body: open a fresh connection, press button 1, record the pid,
/// then block until SIGTERM releases the button and exits.
fn run_holder(pidfile: &std::path::Path) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RELEASE: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_term(_sig: i32) {
        RELEASE.store(true, Ordering::SeqCst);
    }
    // SAFETY: installing a trivial async-signal-safe handler (atomic store only).
    unsafe {
        install_handler(SIGTERM, on_term);
        install_handler(SIGINT, on_term);
    }

    let conn = Conn::connect()?;
    let mut queue = conn.conn.new_event_queue::<PointerState>();
    let qh = queue.handle();
    let ptr = Pointer::create(&conn, &qh)?;

    // Press and hold at the current position (motion is a no-op warp to keep the
    // compositor's pointer where it is; virtual pointers start at 0,0 otherwise,
    // so we leave motion out and just press - callers move first via `click`/JS
    // `_moveMouse`, which warps before mouseDown).
    ptr.button(BTN_LEFT, true);
    ptr.frame();
    queue
        .roundtrip(&mut PointerState)
        .context("holder press roundtrip")?;

    // Record our pid now that the button is actually held.
    std::fs::write(pidfile, std::process::id().to_string())
        .context("failed to write holder pidfile")?;

    // Block until signalled, servicing the Wayland connection so it stays alive.
    while !RELEASE.load(Ordering::SeqCst) {
        queue.flush().ok();
        thread::sleep(Duration::from_millis(50));
    }

    // Release the button explicitly before we disconnect.
    ptr.button(BTN_LEFT, false);
    ptr.frame();
    queue.roundtrip(&mut PointerState).ok();
    let _ = std::fs::remove_file(pidfile);
    Ok(())
}

/// Release a held left button by signalling the holder process.
pub fn left_mouse_up(_conn: &Conn) -> Result<ButtonStateResult> {
    let pidfile = holder_pidfile();
    if let Some(pid) = read_pid(&pidfile) {
        let _ = signal_pid(pid, SIGTERM);
        // Give the holder a moment to release + clean up its pidfile.
        for _ in 0..50 {
            if read_pid(&pidfile).is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&pidfile);
    }
    // Idempotent: releasing when nothing is held is not an error.
    Ok(ButtonStateResult {
        action: "left-mouse-up".to_owned(),
        button: "left".to_owned(),
        is_held: false,
    })
}

/// Read + validate the holder pid from the pidfile (None if absent/dead/garbage).
fn read_pid(path: &std::path::Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    if pid <= 1 {
        return None;
    }
    // kill(pid, 0) probes existence without sending a signal.
    if signal_pid(pid, 0).is_ok() {
        Some(pid)
    } else {
        None
    }
}

// --- Minimal libc bindings (no libc crate; symbols exist in glibc + musl). ---

unsafe extern "C" {
    fn fork() -> i32;
    fn setsid() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Send `sig` to `pid`; `sig == 0` probes existence.
fn signal_pid(pid: i32, sig: i32) -> Result<()> {
    // SAFETY: kill with a plausible pid + signal number; errors surface via -1.
    let rc = unsafe { kill(pid, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("kill failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_parse_and_evdev() {
        assert!(matches!(Button::parse("left").unwrap(), Button::Left));
        assert!(matches!(Button::parse("right").unwrap(), Button::Right));
        assert!(matches!(Button::parse("middle").unwrap(), Button::Middle));
        assert!(Button::parse("wheel").is_err());
        assert_eq!(Button::Left.evdev(), BTN_LEFT);
        assert_eq!(Button::Right.evdev(), BTN_RIGHT);
        assert_eq!(Button::Middle.evdev(), BTN_MIDDLE);
    }

    #[test]
    fn pidfile_respects_profile() {
        // Default profile: no suffix.
        // SAFETY: single-threaded test; we set/unset our own env var.
        unsafe {
            std::env::remove_var("CLAUDE_PROFILE");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let p = holder_pidfile();
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/user/1000/wlroots-bridge-lmb.pid")
        );

        unsafe {
            std::env::set_var("CLAUDE_PROFILE", "work");
        }
        let p = holder_pidfile();
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/user/1000/wlroots-bridge-lmb-work.pid")
        );
        unsafe {
            std::env::remove_var("CLAUDE_PROFILE");
        }
    }

    #[test]
    fn read_pid_rejects_garbage() {
        let dir = std::env::temp_dir();
        let f = dir.join(format!("wlroots-bridge-test-{}.pid", std::process::id()));
        std::fs::write(&f, "not-a-number").unwrap();
        assert_eq!(read_pid(&f), None);
        std::fs::write(&f, "0").unwrap();
        assert_eq!(read_pid(&f), None);
        // Our own pid exists -> Some.
        std::fs::write(&f, std::process::id().to_string()).unwrap();
        assert_eq!(read_pid(&f), Some(std::process::id() as i32));
        std::fs::remove_file(&f).ok();
    }

    #[test]
    fn scroll_step_matches_convention() {
        assert_eq!(SCROLL_STEP, 15.0);
    }

    /// What wlroots' wlr_cursor does with a motion_absolute event: map the
    /// normalized position onto the layout bounding box.
    fn wlroots_lands_at(b: &LayoutBox, x: i32, y: i32) -> (f64, f64) {
        let (ew, eh) = b.extents();
        let (px, py) = b.map_point(x, y);
        (
            b.min_x as f64 + f64::from(px) / f64::from(ew) * (b.max_x - b.min_x) as f64,
            b.min_y as f64 + f64::from(py) / f64::from(eh) * (b.max_y - b.min_y) as f64,
        )
    }

    #[test]
    fn layout_at_origin_is_unchanged() {
        let b = LayoutBox::from_rects([(0, 0, 1280, 720), (1280, 0, 1280, 720)]).unwrap();
        assert_eq!(b.extents(), (2560, 720));
        assert_eq!(b.map_point(100, 100), (100, 100));
        assert_eq!(b.map_point(2000, 700), (2000, 700));
        assert_eq!(wlroots_lands_at(&b, 1500, 300), (1500.0, 300.0));
    }

    #[test]
    fn negative_x_left_monitor_is_reachable() {
        // X11-1 at 0,0 (1280x720 logical), X11-2 at -1280,0.
        let b = LayoutBox::from_rects([(0, 0, 1280, 720), (-1280, 0, 1280, 720)]).unwrap();
        assert_eq!(b.extents(), (2560, 720));
        assert_eq!(b.map_point(100, 100), (1380, 100));
        assert_eq!(b.map_point(-1280, 0), (0, 0));
        assert_eq!(b.map_point(-1, 719), (1279, 719));
        assert_eq!(wlroots_lands_at(&b, 100, 100), (100.0, 100.0));
        assert_eq!(wlroots_lands_at(&b, -640, 360), (-640.0, 360.0));
        assert_eq!(wlroots_lands_at(&b, 1279, 719), (1279.0, 719.0));
    }

    #[test]
    fn vertical_offset_second_output() {
        // X11-2 at 1280,200: box height is 920, y maps 1:1.
        let b = LayoutBox::from_rects([(0, 0, 1280, 720), (1280, 200, 1280, 720)]).unwrap();
        assert_eq!(b.extents(), (2560, 920));
        assert_eq!(b.map_point(1300, 900), (1300, 900));
        assert_eq!(wlroots_lands_at(&b, 1300, 900), (1300.0, 900.0));
        // Output above the origin: negative y is reachable.
        let b = LayoutBox::from_rects([(0, 0, 1280, 720), (0, -720, 1280, 720)]).unwrap();
        assert_eq!(b.map_point(10, -700), (10, 20));
        assert_eq!(wlroots_lands_at(&b, 10, -700), (10.0, -700.0));
    }

    #[test]
    fn gap_layout_maps_exactly() {
        // 200px gap between outputs: the box spans it; points on either side
        // map 1:1.
        let b = LayoutBox::from_rects([(0, 0, 1280, 720), (1480, 0, 1280, 720)]).unwrap();
        assert_eq!(b.extents(), (2760, 720));
        assert_eq!(wlroots_lands_at(&b, 1500, 10), (1500.0, 10.0));
        assert_eq!(wlroots_lands_at(&b, 1279, 10), (1279.0, 10.0));
    }

    #[test]
    fn clamps_to_last_pixel() {
        let b = LayoutBox::from_rects([(-1280, 0, 1280, 720), (0, 0, 1280, 720)]).unwrap();
        assert_eq!(b.map_point(-5000, -5), (0, 0));
        assert_eq!(b.map_point(5000, 5000), (2559, 719));
        assert!(LayoutBox::from_rects(std::iter::empty()).is_none());
    }
}
