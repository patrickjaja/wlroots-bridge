//! Screenshot and region (zoom) capture over `zwlr_screencopy_manager_v1`.
//!
//! This is the protocol `grim` uses. We ask the compositor to copy an output
//! (`capture_output`) into a `wl_shm` buffer, wait for the `ready` event,
//! convert the buffer's pixel layout to packed RGB, then downscale + JPEG-encode
//! exactly like x11-bridge / kwin-portal-bridge (long edge <= 1568 px, total
//! <= 1_150_000 px, JPEG quality 75). For `zoom` we crop the RGB to the region
//! first.
//!
//! ## Coordinate semantics for `zoom`
//!
//! `zoom`'s `(x, y, w, h)` are relative to the selected monitor's top-left
//! (matching x11-bridge / the JS `zoom` caller). We capture the whole output
//! then crop in logical pixels. The screencopy buffer is in the output's
//! physical pixels, so a fractional-scaled output needs the crop scaled by the
//! same physical/logical ratio; we compute it from the buffer size vs the
//! output's logical size.
//!
//! ## Output transforms
//!
//! The screencopy buffer is in *buffer* orientation (the untransformed panel
//! mode). For a rotated or flipped output we first undo the frame's y-invert
//! flag, then apply the `wl_output.transform` (see `crate::transform`) so the
//! image matches the output's logical orientation and size - the orientation
//! `screens` reports and the pointer coordinates use, and what grim produces.

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::conn::Conn;
use crate::mask::{self, MaskReport, MaskRequest};
use crate::output::{Rect, ScreenshotCapture, ScreenshotResult};
use crate::screens::{self, OutputGeometry};
use crate::transform::Transform;
use crate::wincap;

/// Long-edge cap on the encoded image, in physical pixels. Mirrors x11-bridge.
pub const MAX_LONG_EDGE: u32 = 1568;
/// Total-pixel cap on the encoded image. Mirrors x11-bridge.
pub const MAX_PIXELS: u32 = 1_150_000;
/// JPEG quality, matching x11-bridge / kwin-portal-bridge.
const JPEG_QUALITY: u8 = 75;

/// A packed 8-bit-per-channel RGB image (row-major, no padding).
pub(crate) struct RgbImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// `width * height * 3` bytes, R,G,B per pixel.
    pub(crate) pixels: Vec<u8>,
}

impl RgbImage {
    /// A `width x height` image in the mask colour.
    pub(crate) fn mask_canvas(width: u32, height: u32) -> Self {
        let n = (width as usize) * (height as usize) * 3;
        Self {
            width,
            height,
            pixels: mask::MASK_RGB.iter().copied().cycle().take(n).collect(),
        }
    }
}

/// The screencopy buffer format the compositor advertised for an output.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BufferFormat {
    pub(crate) format: wl_shm::Format,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: u32,
}

/// State driving one screencopy frame to completion.
struct CaptureState {
    /// Buffer parameters from the frame's `buffer` event.
    format: Option<BufferFormat>,
    /// Whether the frame reports y-inverted content.
    y_invert: bool,
    /// Set when the `ready` event arrives (copy complete).
    ready: bool,
    /// Set on the `failed` event.
    failed: bool,
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: wayland_client::WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.format = Some(BufferFormat {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::Flags {
                flags: wayland_client::WEnum::Value(flags),
            } => {
                state.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(CaptureState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(CaptureState: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);

/// A memfd-backed shm mapping we can read the copied pixels out of.
pub(crate) struct ShmBuffer {
    #[allow(dead_code)]
    file: std::fs::File,
    pub(crate) mmap: memmap_min::Mmap,
    pool: wl_shm_pool::WlShmPool,
    pub(crate) buffer: wl_buffer::WlBuffer,
}

/// Minimal read-only mmap helper (no external mmap crate; keeps deps pure Rust).
pub(crate) mod memmap_min {
    use anyhow::{Context, Result};
    use std::os::fd::AsRawFd;

    /// A read-only memory map of a file, unmapped on drop.
    pub struct Mmap {
        ptr: *mut libc_min::c_void,
        len: usize,
    }

    // The mapping is only ever read, and the process is single-threaded per
    // command; sharing across the (single) queue dispatch is safe.
    unsafe impl Send for Mmap {}
    unsafe impl Sync for Mmap {}

    impl Mmap {
        pub fn new(file: &std::fs::File, len: usize) -> Result<Self> {
            // SAFETY: mmap over a valid fd with a length matching the file we
            // just ftruncate'd; PROT_READ | MAP_SHARED so the compositor's
            // writes are visible.
            let ptr = unsafe {
                libc_min::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc_min::PROT_READ,
                    libc_min::MAP_SHARED,
                    file.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc_min::MAP_FAILED {
                return Err(std::io::Error::last_os_error()).context("mmap failed");
            }
            Ok(Self { ptr, len })
        }

        pub fn as_slice(&self) -> &[u8] {
            // SAFETY: ptr/len describe a valid PROT_READ mapping for the map's
            // lifetime.
            unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }

    impl Drop for Mmap {
        fn drop(&mut self) {
            // SAFETY: unmapping the region we created; ignore errors on drop.
            unsafe {
                libc_min::munmap(self.ptr, self.len);
            }
        }
    }

    /// The three libc calls we need, declared directly so we don't pull in the
    /// `libc` crate (keeps the dependency set pure-first-party). These symbols
    /// exist in both glibc and musl.
    #[allow(non_camel_case_types)]
    mod libc_min {
        pub type c_void = core::ffi::c_void;
        pub type size_t = usize;
        pub type off_t = i64;
        pub const PROT_READ: i32 = 0x1;
        pub const MAP_SHARED: i32 = 0x1;
        pub const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

        unsafe extern "C" {
            pub fn mmap(
                addr: *mut c_void,
                length: size_t,
                prot: i32,
                flags: i32,
                fd: i32,
                offset: off_t,
            ) -> *mut c_void;
            pub fn munmap(addr: *mut c_void, length: size_t) -> i32;
        }
    }
}

impl ShmBuffer {
    /// Allocate a memfd of `size` bytes and wrap it in a `wl_shm_pool` + buffer.
    pub(crate) fn new<D>(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<D>,
        fmt: BufferFormat,
    ) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let size = (fmt.stride as usize) * (fmt.height as usize);
        anyhow::ensure!(
            size > 0 && size <= i32::MAX as usize,
            "shm buffer of {size} bytes does not fit a wl_shm pool"
        );
        let file = create_sealed_memfd(size)?;

        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            fmt.width as i32,
            fmt.height as i32,
            fmt.stride as i32,
            fmt.format,
            qh,
            (),
        );

        let mmap = memmap_min::Mmap::new(&file, size)?;

        Ok(Self {
            file,
            mmap,
            pool,
            buffer,
        })
    }

    /// Tell the compositor to free the buffer and its pool (the mapping stays
    /// readable until drop).
    pub(crate) fn destroy(&self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Create an anonymous, writable file of `size` bytes for the shm pool.
///
/// Uses `memfd_create` via the syscall (no libc crate). Falls back to a
/// tmpfile in `$XDG_RUNTIME_DIR` if memfd is unavailable.
fn create_sealed_memfd(size: usize) -> Result<std::fs::File> {
    use std::io::Write;

    // Try memfd_create(2) directly.
    let name = c"wlroots-bridge-shm";
    // SAFETY: memfd_create with a valid NUL-terminated name and no flags.
    let fd = unsafe { memfd_create(name.as_ptr(), 0) };
    let file = if fd >= 0 {
        // SAFETY: fd is a fresh owned descriptor from memfd_create.
        use std::os::fd::FromRawFd;
        unsafe { std::fs::File::from_raw_fd(fd) }
    } else {
        // Fallback: a normal temp file.
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        let path = format!("{dir}/wlroots-bridge-shm-{}", std::process::id());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .context("failed to create shm fallback file")?;
        let _ = std::fs::remove_file(&path); // unlink; keep the open fd
        file
    };

    // Size the file to the pool size.
    file.set_len(size as u64)
        .context("failed to size the shm file")?;
    // Touch the last byte so the mapping is backed (belt-and-suspenders).
    let mut f = &file;
    let _ = f.write_all(&[]);

    Ok(file)
}

// memfd_create(2): glibc/musl both export it since glibc 2.27 / musl 1.1.20.
unsafe extern "C" {
    fn memfd_create(name: *const core::ffi::c_char, flags: core::ffi::c_uint) -> i32;
}

/// Upper bound for the compositor to deliver `ready` after `copy`.
const COPY_TIMEOUT: Duration = Duration::from_secs(2);

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x001;

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: core::ffi::c_ulong, timeout: i32) -> i32;
}

/// Dispatch events until the frame is `ready` or `failed`, or `timeout`
/// elapses. Returns true when the frame completed (ready), false on timeout or
/// failure (the caller tells them apart through `state.failed`). Checks the
/// state before any blocking, so an already-delivered event costs no wait.
fn wait_for_frame(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    timeout: Duration,
) -> Result<bool> {
    wait_until(conn, queue, state, Instant::now() + timeout, |s| {
        s.ready || s.failed
    })?;
    Ok(state.ready)
}

/// Dispatch events until `done(state)` holds or `deadline` passes. Returns
/// whether `done` held. Checks the state before any blocking, so an
/// already-delivered event costs no wait.
pub(crate) fn wait_until<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
    done: impl Fn(&S) -> bool,
) -> Result<bool> {
    loop {
        queue.dispatch_pending(state)?;
        if done(state) {
            return Ok(true);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(false);
        };
        conn.flush()?;
        // None means events are already queued: loop to dispatch them.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut pfd = PollFd {
            fd: guard.connection_fd().as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        // Round up so a sub-millisecond remainder cannot spin.
        let ms = remaining.as_millis().min((i32::MAX - 1) as u128) as i32 + 1;
        // SAFETY: `pfd` is a valid, initialised pollfd for the duration of the call.
        let n = unsafe { poll(&mut pfd, 1, ms) };
        if n > 0 {
            match guard.read() {
                Ok(_) => {}
                // A read that yielded only a partial message: wait for more.
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        } else if n < 0 {
            let err = std::io::Error::last_os_error();
            drop(guard);
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err).context("poll on the Wayland socket failed");
            }
        } else {
            // Timeout: drop the guard (cancels the read) and re-check the deadline.
            drop(guard);
        }
    }
}

/// Capture a full output's pixels as packed RGB in the output's logical
/// orientation, plus those (transformed, physical-pixel) dimensions.
fn capture_output_rgb(conn: &Conn, geom: &OutputGeometry) -> Result<(RgbImage, u32, u32)> {
    // Re-enumerate to get the live wl_output proxy for the target output.
    let entries = screens::enumerate(conn)?;
    let entry = entries
        .into_iter()
        .find(|e| e.registry_name == geom.registry_name)
        .ok_or_else(|| anyhow::anyhow!("output vanished during capture"))?;

    let mut queue = conn.conn.new_event_queue::<CaptureState>();
    let qh = queue.handle();

    let manager = conn.bind_screencopy_manager(&qh)?;
    let shm = conn.bind_shm(&qh)?;

    // overlay_cursor=0: exclude the cursor from the shot (matches grim default).
    let frame = manager.capture_output(0, &entry.wl_output, &qh, ());

    let mut state = CaptureState {
        format: None,
        y_invert: false,
        ready: false,
        failed: false,
    };

    // First roundtrip: receive the `buffer` event describing the format.
    queue
        .roundtrip(&mut state)
        .context("failed the screencopy buffer roundtrip")?;

    let fmt = state
        .format
        .ok_or_else(|| anyhow::anyhow!("compositor sent no screencopy buffer format"))?;

    // Allocate the shm buffer and request the copy.
    let shm_buffer = ShmBuffer::new(&shm, &qh, fmt)?;
    frame.copy(&shm_buffer.buffer);

    // Wait until ready or failed. Some compositors (Hyprland at 4K, others
    // under load) copy into the buffer on their next rendered frame, which can
    // be well after a sync roundtrip returns. Wait on the socket against a
    // wall-clock deadline; a compositor that answers at once returns at once.
    let started = Instant::now();
    let outcome = wait_for_frame(&conn.conn, &mut queue, &mut state, COPY_TIMEOUT)
        .context("failed a screencopy copy roundtrip")?;
    if state.failed {
        bail!("compositor reported screencopy failure for the output");
    }
    if !outcome {
        bail!(
            "screencopy did not complete (no ready event after {} ms)",
            started.elapsed().as_millis()
        );
    }

    let data = shm_buffer.mmap.as_slice();
    let rgb = decode_shm(data, fmt, state.y_invert)?;
    let rgb = apply_output_transform(rgb, entry.transform);
    let (w, h) = (rgb.width, rgb.height);
    Ok((rgb, w, h))
}

/// Budget for one compositor IPC snapshot in mask mode.
const MASK_IPC_DEADLINE: Duration = Duration::from_millis(1500);

/// Capture an output and apply `mask` (see `crate::mask`). `output` is the
/// output's logical rect, the area the image shows.
///
/// Keep mode captures each kept window on its own where the compositor can
/// (`crate::wincap`). Otherwise it captures the output, reads the IPC before
/// and after the copy and accepts the frame only when both give the same plan
/// (one retry), so a window that moved, opened or closed during the copy
/// cannot leak through a stale plan.
fn capture_masked(
    conn: &Conn,
    geom: &OutputGeometry,
    output: &Rect,
    mask: &MaskRequest,
) -> Result<(RgbImage, Option<MaskReport>)> {
    let ids = match mask {
        MaskRequest::None => return Ok((capture_output_rgb(conn, geom)?.0, None)),
        MaskRequest::All => {
            let rgb = capture_output_rgb(conn, geom)?.0;
            let report = MaskReport {
                mode: "all",
                capture: None,
                kept: Vec::new(),
                regions: Vec::new(),
                children: None,
                children_skipped: None,
            };
            return Ok((apply_keep_mask(&rgb, &[]), Some(report)));
        }
        MaskRequest::Keep(ids) => ids,
    };
    let kind =
        crate::ipc::kind().context("masked capture needs the compositor IPC (sway or Hyprland)")?;
    let avail = wincap::Avail::from_globals(&conn.globals);
    let upright = geom.transform == Transform::Normal;
    let path = wincap::select_env(avail, kind, upright)?;
    // One session per call: binding the toplevel manager a second time on
    // this connection would give the handles new ids (the `windows` ids are
    // the first binding's), so the fallback below reuses it.
    let mut sess = if path != wincap::Path::Output || avail.mapping {
        Some(wincap::Session::open(
            conn,
            path == wincap::Path::Ext,
            avail,
        )?)
    } else {
        None
    };
    // A window buffer that does not match its rect: the output cut, below.
    if path != wincap::Path::Output
        && let Some(s) = sess.as_mut()
        && let Some((rgb, report)) = wincap::capture_keep(conn, s, geom, output, ids, path)?
    {
        return Ok((rgb, Some(report)));
    }

    // Window identity by address where the compositor maps handles to
    // windows, else by app id + title (`crate::mask`).
    let (toplevels, by_address) = match &sess {
        Some(s) => (s.toplevels(), s.by_address()),
        None => (crate::wm::collect_toplevels(conn)?, false),
    };
    let ctx = mask::PlanContext {
        output,
        mode: mask::CaptureMode::Output,
        by_address,
        children_allowed: false,
    };
    let read_plan = |sess: &mut Option<wincap::Session>| -> Result<mask::Plan> {
        if let Some(s) = sess {
            s.check_kept_open(ids)?;
        }
        let snap = crate::ipc::snapshot(true, MASK_IPC_DEADLINE)
            .context("masked capture needs the compositor IPC (sway or Hyprland)")?;
        mask::plan(&toplevels, ids, &snap, ctx)
    };
    for _ in 0..2 {
        let before = read_plan(&mut sess)?;
        let rgb = capture_output_rgb(conn, geom)?.0;
        let after = read_plan(&mut sess)?;
        if !before.same_image(&after, mask::CaptureMode::Output) {
            continue;
        }
        let px = mask::to_pixels(&after.keep, output, rgb.width, rgb.height);
        let report = MaskReport {
            mode: "keep",
            capture: Some(mask::CaptureMode::Output.as_str()),
            kept: after.kept,
            regions: after.keep,
            children: None,
            children_skipped: None,
        };
        return Ok((apply_keep_mask(&rgb, &px), Some(report)));
    }
    bail!("windows changed while capturing (twice); masked capture refused")
}

/// A copy of `src` painted [`mask::MASK_RGB`] everywhere except `keep`.
fn apply_keep_mask(src: &RgbImage, keep: &[mask::PixRect]) -> RgbImage {
    let mut pixels: Vec<u8> = mask::MASK_RGB
        .iter()
        .copied()
        .cycle()
        .take(src.pixels.len())
        .collect();
    let w = src.width as usize;
    for k in keep {
        let (x0, x1) = (k.x0.min(src.width) as usize, k.x1.min(src.width) as usize);
        let (y0, y1) = (k.y0.min(src.height) as usize, k.y1.min(src.height) as usize);
        for y in y0..y1 {
            let a = (y * w + x0) * 3;
            let b = (y * w + x1) * 3;
            pixels[a..b].copy_from_slice(&src.pixels[a..b]);
        }
    }
    RgbImage {
        width: src.width,
        height: src.height,
        pixels,
    }
}

/// Capture a full monitor. `display` selects the monitor (default: primary).
pub fn screenshot(
    conn: &Conn,
    display: Option<&str>,
    mask: &MaskRequest,
) -> Result<ScreenshotResult> {
    let entries = screens::enumerate(conn)?;
    let entry = screens::resolve_output(&entries, display)?;
    let geom = OutputGeometry::from_entry(&entry);
    let screen = screens::to_screen(&geom, true);

    let (rgb, report) = capture_masked(conn, &geom, &screen.geometry, mask)?;
    let target = compute_target_dims(rgb.width, rgb.height);
    let encoded = encode_resized_jpeg_base64(&rgb, target)?;

    Ok(ScreenshotResult {
        base64: encoded.base64,
        width: encoded.width,
        height: encoded.height,
        display_width: screen.geometry.width.max(0) as u32,
        display_height: screen.geometry.height.max(0) as u32,
        display_id: screen.id,
        origin_x: screen.geometry.x,
        origin_y: screen.geometry.y,
        mask: report,
    })
}

/// Capture a logical region `(x, y, w, h)` within a monitor. Coordinates are
/// relative to the monitor's top-left, in logical pixels.
pub fn zoom(
    conn: &Conn,
    display: Option<&str>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    mask: &MaskRequest,
) -> Result<ScreenshotCapture> {
    if w <= 0 || h <= 0 {
        bail!("zoom region must have positive width and height");
    }

    let entries = screens::enumerate(conn)?;
    let entry = screens::resolve_output(&entries, display)?;
    let geom = OutputGeometry::from_entry(&entry);
    let screen = screens::to_screen(&geom, true);

    let (rgb, report) = capture_masked(conn, &geom, &screen.geometry, mask)?;
    let (phys_w, phys_h) = (rgb.width, rgb.height);

    // The buffer is in physical pixels; the region is logical. Scale the crop by
    // the physical/logical ratio so a fractional-scaled output crops correctly.
    let logical_w = screen.geometry.width.max(1) as f64;
    let logical_h = screen.geometry.height.max(1) as f64;
    let sx = phys_w as f64 / logical_w;
    let sy = phys_h as f64 / logical_h;

    let px = (x.max(0) as f64 * sx).round() as i32;
    let py = (y.max(0) as f64 * sy).round() as i32;
    let pw = (w as f64 * sx).round() as i32;
    let ph = (h as f64 * sy).round() as i32;

    let cropped = crop_rgb(&rgb, px, py, pw, ph)?;
    let target = compute_target_dims(cropped.width, cropped.height);
    let mut out = encode_resized_jpeg_base64(&cropped, target)?;
    out.mask = report;
    Ok(out)
}

/// Convert a screencopy shm buffer to packed RGB.
///
/// Handles the formats wlroots compositors commonly hand out: XRGB8888 /
/// ARGB8888 (BGRx / BGRA in memory, little-endian) and XBGR8888 / ABGR8888
/// (RGBx / RGBA). Honours the frame's y-invert flag by reading rows bottom-up.
pub(crate) fn decode_shm(data: &[u8], fmt: BufferFormat, y_invert: bool) -> Result<RgbImage> {
    let w = fmt.width as usize;
    let h = fmt.height as usize;
    let stride = fmt.stride as usize;
    let needed = stride
        .checked_mul(h)
        .context("screencopy buffer dimensions overflow")?;
    if data.len() < needed {
        bail!(
            "screencopy buffer is {} bytes, expected at least {needed} for {}x{}",
            data.len(),
            fmt.width,
            fmt.height
        );
    }

    // Byte offsets of R, G, B within each little-endian 4-byte pixel.
    let (ro, go, bo) = match fmt.format {
        // XRGB8888 / ARGB8888: 0xAARRGGBB packed little-endian -> [B, G, R, A].
        wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888 => (2usize, 1usize, 0usize),
        // XBGR8888 / ABGR8888: 0xAABBGGRR -> [R, G, B, A].
        wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => (0usize, 1usize, 2usize),
        other => bail!("unsupported screencopy pixel format {other:?}"),
    };

    let mut pixels = vec![0u8; w * h * 3];
    for dst_row in 0..h {
        let src_row = if y_invert { h - 1 - dst_row } else { dst_row };
        let src = &data[src_row * stride..src_row * stride + w * 4];
        let dst = &mut pixels[dst_row * w * 3..(dst_row + 1) * w * 3];
        for col in 0..w {
            let s = col * 4;
            let d = col * 3;
            dst[d] = src[s + ro];
            dst[d + 1] = src[s + go];
            dst[d + 2] = src[s + bo];
        }
    }

    Ok(RgbImage {
        width: fmt.width,
        height: fmt.height,
        pixels,
    })
}

/// Re-orient a buffer-oriented image to the output's logical orientation.
fn apply_output_transform(src: RgbImage, transform: Transform) -> RgbImage {
    if transform == Transform::Normal {
        return src;
    }
    let (bw, bh) = (src.width, src.height);
    let (lw, lh) = transform.transformed_size(bw, bh);
    let mut pixels = vec![0u8; src.pixels.len()];
    for by in 0..bh {
        for bx in 0..bw {
            let (lx, ly) = transform.buffer_to_logical(bx, by, bw, bh);
            let s = ((by * bw + bx) as usize) * 3;
            let d = ((ly * lw + lx) as usize) * 3;
            pixels[d..d + 3].copy_from_slice(&src.pixels[s..s + 3]);
        }
    }
    RgbImage {
        width: lw,
        height: lh,
        pixels,
    }
}

/// Crop a packed-RGB image to `(x, y, w, h)`, clamping to the image bounds.
fn crop_rgb(src: &RgbImage, x: i32, y: i32, w: i32, h: i32) -> Result<RgbImage> {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w).max(0) as u32).min(src.width);
    let y1 = ((y + h).max(0) as u32).min(src.height);
    if x1 <= x0 || y1 <= y0 {
        bail!("zoom region does not intersect the captured output");
    }
    let cw = x1 - x0;
    let ch = y1 - y0;

    let mut pixels = vec![0u8; (cw as usize) * (ch as usize) * 3];
    for row in 0..ch {
        let sy = (y0 + row) as usize;
        let src_off = (sy * src.width as usize + x0 as usize) * 3;
        let dst_off = (row as usize * cw as usize) * 3;
        let len = cw as usize * 3;
        pixels[dst_off..dst_off + len].copy_from_slice(&src.pixels[src_off..src_off + len]);
    }

    Ok(RgbImage {
        width: cw,
        height: ch,
        pixels,
    })
}

/// x11-bridge / kwin downscale math: cap long edge and total pixels, keep aspect
/// ratio, never upscale.
fn compute_target_dims(w: u32, h: u32) -> (u32, u32) {
    let phys_w = (w.max(1)) as f64;
    let phys_h = (h.max(1)) as f64;
    let long_edge_scale = (MAX_LONG_EDGE as f64) / phys_w.max(phys_h);
    let pixel_scale = ((MAX_PIXELS as f64) / (phys_w * phys_h)).sqrt();
    let scale = 1.0_f64.min(long_edge_scale).min(pixel_scale);

    (
        (phys_w * scale).round().max(1.0) as u32,
        (phys_h * scale).round().max(1.0) as u32,
    )
}

/// Downscale (or copy) `rgb` to `target` and JPEG-encode to base64.
fn encode_resized_jpeg_base64(rgb: &RgbImage, target: (u32, u32)) -> Result<ScreenshotCapture> {
    let (tw, th) = target;
    let resized = if tw == rgb.width && th == rgb.height {
        RgbImage {
            width: rgb.width,
            height: rgb.height,
            pixels: rgb.pixels.clone(),
        }
    } else {
        resize_area_average(rgb, tw, th)
    };

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    encoder
        .encode(
            &resized.pixels,
            resized.width,
            resized.height,
            image::ExtendedColorType::Rgb8,
        )
        .context("failed to JPEG-encode screenshot")?;

    Ok(ScreenshotCapture {
        base64: base64::engine::general_purpose::STANDARD.encode(&jpeg),
        width: resized.width,
        height: resized.height,
        mask: None,
    })
}

/// Box (area-average) downscale. Ported verbatim from x11-bridge.
fn resize_area_average(src: &RgbImage, dst_w: u32, dst_h: u32) -> RgbImage {
    let dst_w = dst_w.max(1);
    let dst_h = dst_h.max(1);
    let sw = src.width.max(1);
    let sh = src.height.max(1);
    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 3];

    let x_ratio = sw as f64 / dst_w as f64;
    let y_ratio = sh as f64 / dst_h as f64;

    for dy in 0..dst_h {
        let sy0 = (dy as f64 * y_ratio).floor() as u32;
        let sy1 = (((dy + 1) as f64 * y_ratio).ceil() as u32)
            .min(sh)
            .max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx as f64 * x_ratio).floor() as u32;
            let sx1 = (((dx + 1) as f64 * x_ratio).ceil() as u32)
                .min(sw)
                .max(sx0 + 1);

            let mut r = 0u64;
            let mut g = 0u64;
            let mut b = 0u64;
            let mut count = 0u64;
            for sy in sy0..sy1 {
                let row = (sy * sw) as usize * 3;
                for sx in sx0..sx1 {
                    let s = row + sx as usize * 3;
                    r += u64::from(src.pixels[s]);
                    g += u64::from(src.pixels[s + 1]);
                    b += u64::from(src.pixels[s + 2]);
                    count += 1;
                }
            }
            let count = count.max(1);
            let d = ((dy * dst_w + dx) as usize) * 3;
            out[d] = (r / count) as u8;
            out[d + 1] = (g / count) as u8;
            out[d + 2] = (b / count) as u8;
        }
    }

    RgbImage {
        width: dst_w,
        height: dst_h,
        pixels: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb8888_is_bgrx() {
        // One pixel, little-endian XRGB8888 -> [B, G, R, x].
        let data = [0x10u8, 0x20, 0x30, 0xFF];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 1,
            stride: 4,
        };
        let img = decode_shm(&data, fmt, false).unwrap();
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10]); // R, G, B
    }

    #[test]
    fn xbgr8888_is_rgbx() {
        let data = [0x30u8, 0x20, 0x10, 0xFF];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xbgr8888,
            width: 1,
            height: 1,
            stride: 4,
        };
        let img = decode_shm(&data, fmt, false).unwrap();
        assert_eq!(img.pixels, vec![0x30, 0x20, 0x10]);
    }

    #[test]
    fn y_invert_reads_bottom_up() {
        // 1x2, XRGB8888: row0 red, row1 green. With y_invert we read bottom-up
        // so the output top row is green.
        let data = [
            0x00, 0x00, 0xFF, 0xFF, // row0 = red (BGRx)
            0x00, 0xFF, 0x00, 0xFF, // row1 = green
        ];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 2,
            stride: 4,
        };
        let img = decode_shm(&data, fmt, true).unwrap();
        // top row should be green now.
        assert_eq!(&img.pixels[0..3], &[0x00, 0xFF, 0x00]);
        assert_eq!(&img.pixels[3..6], &[0xFF, 0x00, 0x00]);
    }

    #[test]
    fn stride_padding_respected() {
        // 1px wide, stride 8 (4 bytes padding). Two rows.
        let data = [
            0x10, 0x20, 0x30, 0xFF, 0, 0, 0, 0, // row0 pixel + pad
            0x40, 0x50, 0x60, 0xFF, 0, 0, 0, 0, // row1 pixel + pad
        ];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 2,
            stride: 8,
        };
        let img = decode_shm(&data, fmt, false).unwrap();
        assert_eq!(&img.pixels[0..3], &[0x30, 0x20, 0x10]);
        assert_eq!(&img.pixels[3..6], &[0x60, 0x50, 0x40]);
    }

    #[test]
    fn rejects_short_buffer() {
        let data = [0u8; 3];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 1,
            stride: 4,
        };
        assert!(decode_shm(&data, fmt, false).is_err());
    }

    #[test]
    fn rejects_unknown_format() {
        let data = [0u8; 4];
        let fmt = BufferFormat {
            format: wl_shm::Format::C8,
            width: 1,
            height: 1,
            stride: 4,
        };
        assert!(decode_shm(&data, fmt, false).is_err());
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> RgbImage {
        let mut pixels = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..(w * h) {
            pixels.extend_from_slice(&rgb);
        }
        RgbImage {
            width: w,
            height: h,
            pixels,
        }
    }

    #[test]
    fn crop_clamps_and_extracts() {
        // 4x4 image; crop a 2x2 at (1,1).
        let mut img = solid(4, 4, [0, 0, 0]);
        // paint pixel (2,2) white so we can verify the crop origin.
        let idx = (2 * 4 + 2) * 3;
        img.pixels[idx] = 255;
        img.pixels[idx + 1] = 255;
        img.pixels[idx + 2] = 255;

        let c = crop_rgb(&img, 1, 1, 2, 2).unwrap();
        assert_eq!(c.width, 2);
        assert_eq!(c.height, 2);
        // (2,2) in source is (1,1) in the crop.
        let cidx = (2 + 1) * 3;
        assert_eq!(&c.pixels[cidx..cidx + 3], &[255, 255, 255]);
    }

    #[test]
    fn keep_mask_paints_everything_else() {
        let img = solid(4, 3, [200, 100, 50]);
        let out = apply_keep_mask(
            &img,
            &[mask::PixRect {
                x0: 1,
                y0: 1,
                x1: 3,
                y1: 2,
            }],
        );
        for y in 0..3u32 {
            for x in 0..4u32 {
                let i = ((y * 4 + x) * 3) as usize;
                let kept = (1..3).contains(&x) && y == 1;
                let want = if kept { [200, 100, 50] } else { mask::MASK_RGB };
                assert_eq!(&out.pixels[i..i + 3], &want, "({x},{y})");
            }
        }
        // Nothing kept: all mask color.
        let all = apply_keep_mask(&img, &[]);
        assert!(all.pixels.chunks(3).all(|p| p == mask::MASK_RGB));
    }

    #[test]
    fn crop_outside_errors() {
        let img = solid(4, 4, [0, 0, 0]);
        assert!(crop_rgb(&img, 100, 100, 10, 10).is_err());
    }

    #[test]
    fn target_dims_no_downscale_when_small() {
        assert_eq!(compute_target_dims(800, 600), (800, 600));
    }

    #[test]
    fn target_dims_pixel_cap_1080p() {
        let (w, h) = compute_target_dims(1920, 1080);
        assert!((w * h) <= MAX_PIXELS + 2000);
        assert!(w <= MAX_LONG_EDGE);
        let ratio = w as f64 / h as f64;
        assert!((ratio - (1920.0 / 1080.0)).abs() < 0.02, "ratio {ratio}");
    }

    #[test]
    fn resize_half_averages() {
        let src = solid(2, 2, [100, 100, 100]);
        let out = resize_area_average(&src, 1, 1);
        assert_eq!(out.pixels, vec![100, 100, 100]);
    }

    #[test]
    fn resize_half_mixed_averages() {
        let src = RgbImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 0, 255, 255, 255],
        };
        let out = resize_area_average(&src, 1, 1);
        assert_eq!(out.pixels, vec![127, 127, 127]);
    }

    /// A 3x2 buffer whose pixel values encode their buffer position:
    /// [a b c]
    /// [d e f]   (each pixel = [index, 0, 0]).
    fn labelled_3x2() -> RgbImage {
        let mut pixels = Vec::new();
        for i in 0..6u8 {
            pixels.extend_from_slice(&[i, 0, 0]);
        }
        RgbImage {
            width: 3,
            height: 2,
            pixels,
        }
    }

    fn labels(img: &RgbImage) -> Vec<u8> {
        img.pixels.chunks(3).map(|p| p[0]).collect()
    }

    #[test]
    fn output_transform_all_eight() {
        // Expected logical images (row-major labels) for buffer
        //   0 1 2
        //   3 4 5
        // following wlroots wlr_box_transform(T) on the buffer.
        let cases: [(Transform, (u32, u32), [u8; 6]); 8] = [
            (Transform::Normal, (3, 2), [0, 1, 2, 3, 4, 5]),
            // 90: rotate clockwise.
            (Transform::Rot90, (2, 3), [3, 0, 4, 1, 5, 2]),
            (Transform::Rot180, (3, 2), [5, 4, 3, 2, 1, 0]),
            // 270: rotate counter-clockwise.
            (Transform::Rot270, (2, 3), [2, 5, 1, 4, 0, 3]),
            // flipped: mirror around the vertical axis.
            (Transform::Flipped, (3, 2), [2, 1, 0, 5, 4, 3]),
            // flipped-90: transpose.
            (Transform::Flipped90, (2, 3), [0, 3, 1, 4, 2, 5]),
            // flipped-180: mirror around the horizontal axis.
            (Transform::Flipped180, (3, 2), [3, 4, 5, 0, 1, 2]),
            // flipped-270: anti-transpose.
            (Transform::Flipped270, (2, 3), [5, 2, 4, 1, 3, 0]),
        ];
        for (t, dims, expected) in cases {
            let out = apply_output_transform(labelled_3x2(), t);
            assert_eq!((out.width, out.height), dims, "{t:?} dims");
            assert_eq!(labels(&out), expected.to_vec(), "{t:?} pixels");
        }
    }

    #[test]
    fn y_invert_is_undone_before_the_output_transform() {
        // 1x2 buffer stored bottom-up (y_invert): memory row0 = green, row1 =
        // red, so the true buffer is red-over-green. Rot90 of a 1x2 red/green
        // column is a 2x1 row [green, red] (clockwise: bottom goes left).
        let data = [
            0x00, 0xFF, 0x00, 0xFF, // memory row0 = green (BGRx)
            0x00, 0x00, 0xFF, 0xFF, // memory row1 = red
        ];
        let fmt = BufferFormat {
            format: wl_shm::Format::Xrgb8888,
            width: 1,
            height: 2,
            stride: 4,
        };
        let img = apply_output_transform(decode_shm(&data, fmt, true).unwrap(), Transform::Rot90);
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixels, vec![0x00, 0xFF, 0x00, 0xFF, 0x00, 0x00]);
    }
}
