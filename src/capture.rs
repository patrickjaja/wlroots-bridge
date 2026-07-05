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

use anyhow::{Context, Result, bail};
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use std::os::fd::AsFd;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::conn::Conn;
use crate::output::{ScreenshotCapture, ScreenshotResult};
use crate::screens::{self, OutputGeometry};

/// Long-edge cap on the encoded image, in physical pixels. Mirrors x11-bridge.
pub const MAX_LONG_EDGE: u32 = 1568;
/// Total-pixel cap on the encoded image. Mirrors x11-bridge.
pub const MAX_PIXELS: u32 = 1_150_000;
/// JPEG quality, matching x11-bridge / kwin-portal-bridge.
const JPEG_QUALITY: u8 = 75;

/// A packed 8-bit-per-channel RGB image (row-major, no padding).
struct RgbImage {
    width: u32,
    height: u32,
    /// `width * height * 3` bytes, R,G,B per pixel.
    pixels: Vec<u8>,
}

/// The screencopy buffer format the compositor advertised for an output.
#[derive(Debug, Clone, Copy)]
struct BufferFormat {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
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
struct ShmBuffer {
    #[allow(dead_code)]
    file: std::fs::File,
    mmap: memmap_min::Mmap,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

/// Minimal read-only mmap helper (no external mmap crate; keeps deps pure Rust).
mod memmap_min {
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
    fn new<D>(shm: &wl_shm::WlShm, qh: &QueueHandle<D>, fmt: BufferFormat) -> Result<Self>
    where
        D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
    {
        let size = (fmt.stride as usize) * (fmt.height as usize);
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
            _pool: pool,
            buffer,
        })
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

/// Capture a full output's pixels as packed RGB, plus its physical dimensions.
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

    // Pump until ready or failed.
    for _ in 0..200 {
        queue
            .roundtrip(&mut state)
            .context("failed a screencopy copy roundtrip")?;
        if state.ready || state.failed {
            break;
        }
    }
    if state.failed {
        bail!("compositor reported screencopy failure for the output");
    }
    if !state.ready {
        bail!("screencopy did not complete (no ready event)");
    }

    let data = shm_buffer.mmap.as_slice();
    let rgb = decode_shm(data, fmt, state.y_invert)?;
    Ok((rgb, fmt.width, fmt.height))
}

/// Capture a full monitor. `display` selects the monitor (default: primary).
pub fn screenshot(conn: &Conn, display: Option<&str>) -> Result<ScreenshotResult> {
    let entries = screens::enumerate(conn)?;
    let entry = screens::resolve_output(&entries, display)?;
    let geom = OutputGeometry::from_entry(&entry);
    let screen = screens::to_screen(&geom, true);

    let (rgb, _phys_w, _phys_h) = capture_output_rgb(conn, &geom)?;
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
) -> Result<ScreenshotCapture> {
    if w <= 0 || h <= 0 {
        bail!("zoom region must have positive width and height");
    }

    let entries = screens::enumerate(conn)?;
    let entry = screens::resolve_output(&entries, display)?;
    let geom = OutputGeometry::from_entry(&entry);
    let screen = screens::to_screen(&geom, true);

    let (rgb, phys_w, phys_h) = capture_output_rgb(conn, &geom)?;

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
    encode_resized_jpeg_base64(&cropped, target)
}

/// Convert a screencopy shm buffer to packed RGB.
///
/// Handles the formats wlroots compositors commonly hand out: XRGB8888 /
/// ARGB8888 (BGRx / BGRA in memory, little-endian) and XBGR8888 / ABGR8888
/// (RGBx / RGBA). Honours the frame's y-invert flag by reading rows bottom-up.
fn decode_shm(data: &[u8], fmt: BufferFormat, y_invert: bool) -> Result<RgbImage> {
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
}
