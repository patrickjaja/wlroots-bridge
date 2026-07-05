//! Keyboard synthesis over `zwp_virtual_keyboard_v1`.
//!
//! The protocol requires uploading an XKB keymap fd before sending keycodes; the
//! compositor compiles the text. We generate that text ourselves
//! ([`crate::input::keymap::generate_keymap`]) so the binary never links
//! libxkbcommon (pure-Rust / static-musl). For each command we:
//!
//! 1. resolve the requested keysyms (base key of a spec, or each char of a
//!    `type`), build a keymap binding them + the four modifier keys to
//!    sequential keycodes,
//! 2. write the text to a memfd, upload it via `keymap`,
//! 3. press modifiers (real keycodes, driven via `modifier_map`), tap the base
//!    key(s), release in reverse.
//!
//! `key` events carry Linux evdev codes (XKB keycode - 8), which is exactly what
//! `generate_keymap` returns in each placement's `evdev_code`.

use std::io::Write;
use std::os::fd::AsFd;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wayland_client::QueueHandle;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};

use crate::conn::Conn;
use crate::input::keymap::{self, GeneratedKeymap};
use crate::output::{KeyboardActionResult, TypeActionResult};

/// wl_keyboard key states (the virtual keyboard reuses these values).
const KEY_RELEASED: u32 = 0;
const KEY_PRESSED: u32 = 1;
/// XKB keymap format: 1 = xkb_v1 text.
const XKB_V1: u32 = 1;
/// Small settle delay between key-sequence repeats (matches JS `sleep 0.008`).
const KEY_REPEAT_MS: u64 = 8;

struct KeyboardState;
wayland_client::delegate_noop!(KeyboardState: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
wayland_client::delegate_noop!(KeyboardState: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
wayland_client::delegate_noop!(KeyboardState: ignore wayland_client::protocol::wl_seat::WlSeat);

/// A bound virtual keyboard with a keymap already uploaded.
struct Keyboard {
    kb: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    keymap: GeneratedKeymap,
}

impl Keyboard {
    /// Create a virtual keyboard and upload a keymap binding `keysyms`.
    fn create(conn: &Conn, qh: &QueueHandle<KeyboardState>, keysyms: &[u32]) -> Result<Self> {
        let keymap = keymap::generate_keymap(keysyms);
        let manager = conn.bind_virtual_keyboard_manager(qh)?;
        let seat = conn.bind_seat(qh)?;
        let kb = manager.create_virtual_keyboard(&seat, qh, ());

        // Upload the keymap text (NUL-terminated) via a memfd.
        let mut bytes = keymap.text.clone().into_bytes();
        bytes.push(0); // xkbcommon expects a NUL terminator
        let file = write_memfd(&bytes)?;
        kb.keymap(XKB_V1, file.as_fd(), bytes.len() as u32);

        Ok(Self { kb, keymap })
    }

    fn key(&self, evdev_code: u32, pressed: bool) {
        let state = if pressed { KEY_PRESSED } else { KEY_RELEASED };
        self.kb.key(now_ms(), evdev_code, state);
    }
}

fn now_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}

/// Write `bytes` to an anonymous memfd and return the file (fd stays open).
fn write_memfd(bytes: &[u8]) -> Result<std::fs::File> {
    let name = c"wlroots-bridge-keymap";
    // SAFETY: memfd_create with a valid NUL-terminated name.
    let fd = unsafe { memfd_create(name.as_ptr(), 0) };
    let mut file = if fd >= 0 {
        use std::os::fd::FromRawFd;
        // SAFETY: fresh owned fd from memfd_create.
        unsafe { std::fs::File::from_raw_fd(fd) }
    } else {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        let path = format!("{dir}/wlroots-bridge-keymap-{}", std::process::id());
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .context("failed to create keymap fallback file")?;
        let _ = std::fs::remove_file(&path);
        file
    };
    file.write_all(bytes).context("failed to write keymap")?;
    file.flush().ok();
    Ok(file)
}

unsafe extern "C" {
    fn memfd_create(name: *const core::ffi::c_char, flags: core::ffi::c_uint) -> i32;
}

/// Send an executor-style key sequence such as `ctrl+shift+tab`, `repeat` times.
pub fn key_sequence(conn: &Conn, keys: &str, repeat: Option<u32>) -> Result<KeyboardActionResult> {
    let spec = keymap::parse_spec(keys)?;
    let repeat = repeat.unwrap_or(1).max(1);

    let base_keysym = keymap::keysym_for_name(&spec.base)?;

    let mut queue = conn.conn.new_event_queue::<KeyboardState>();
    let qh = queue.handle();
    let kb = Keyboard::create(conn, &qh, &[base_keysym])?;

    // Resolve modifier evdev codes against the generated keymap.
    let mut modifier_codes = Vec::with_capacity(spec.modifiers.len());
    for name in &spec.modifiers {
        modifier_codes.push(keymap::modifier_evdev_code(&kb.keymap, name)?);
    }
    let base = kb.keymap.placements[0];

    for i in 0..repeat {
        if i > 0 {
            queue.flush().ok();
            thread::sleep(Duration::from_millis(KEY_REPEAT_MS));
        }
        for &code in &modifier_codes {
            kb.key(code, true);
        }
        if base.needs_shift {
            kb.key(kb.keymap.shift_code, true);
        }
        kb.key(base.evdev_code, true);
        kb.key(base.evdev_code, false);
        if base.needs_shift {
            kb.key(kb.keymap.shift_code, false);
        }
        for &code in modifier_codes.iter().rev() {
            kb.key(code, false);
        }
    }

    queue
        .roundtrip(&mut KeyboardState)
        .context("key-sequence roundtrip")?;

    let echoed: Vec<String> = keys
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    Ok(KeyboardActionResult {
        action: "key".to_owned(),
        keys: echoed,
        repeat: Some(repeat),
        duration_ms: None,
    })
}

/// Type `text` as individual key events, `delay_ms` apart.
///
/// Every distinct char in the text gets a keycode in one generated keymap
/// (repeated chars reuse the same placement), so even long Unicode text needs a
/// single keymap upload. XKB keymaps comfortably hold hundreds of keys; should a
/// text exceed the keycode space we page it (re-generate + re-upload).
pub fn type_text(conn: &Conn, text: &str, delay_ms: u64) -> Result<TypeActionResult> {
    // Map each char to a keysym, deduplicating so the keymap stays compact.
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Ok(TypeActionResult {
            action: "type".to_owned(),
            text: text.to_owned(),
            char_count: 0,
        });
    }

    // Build the distinct keysym set (order-preserving) and an index for each char.
    let mut distinct: Vec<u32> = Vec::new();
    let mut char_to_slot: Vec<usize> = Vec::with_capacity(chars.len());
    for &ch in &chars {
        let ks = keymap::keysym_for_char(ch)?;
        let slot = match distinct.iter().position(|&k| k == ks) {
            Some(i) => i,
            None => {
                distinct.push(ks);
                distinct.len() - 1
            }
        };
        char_to_slot.push(slot);
    }

    // XKB keycodes are u8 on the wire in some paths; keep the distinct set within
    // a safe page size and re-upload if a text somehow uses that many glyphs.
    const MAX_KEYS_PER_PAGE: usize = 200;

    let mut queue = conn.conn.new_event_queue::<KeyboardState>();
    let qh = queue.handle();

    let mut char_count = 0usize;

    // Page the distinct keysyms; within a page, type every char whose slot falls
    // in that page. Simpler correct approach: if the whole set fits one page
    // (the overwhelmingly common case), one keyboard + one pass.
    if distinct.len() <= MAX_KEYS_PER_PAGE {
        let kb = Keyboard::create(conn, &qh, &distinct)?;
        for (i, &slot) in char_to_slot.iter().enumerate() {
            if i > 0 && delay_ms > 0 {
                queue.flush().ok();
                thread::sleep(Duration::from_millis(delay_ms));
            }
            tap(&kb, slot);
            char_count += 1;
        }
        queue
            .roundtrip(&mut KeyboardState)
            .context("type roundtrip")?;
    } else {
        // Rare: more than MAX_KEYS_PER_PAGE distinct glyphs. Type char-by-char,
        // rebuilding a single-key keymap per char (correct, just slower).
        for (i, &ch) in chars.iter().enumerate() {
            if i > 0 && delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
            let ks = keymap::keysym_for_char(ch)?;
            let kb = Keyboard::create(conn, &qh, &[ks])?;
            tap(&kb, 0);
            queue
                .roundtrip(&mut KeyboardState)
                .context("type roundtrip")?;
            char_count += 1;
        }
    }

    Ok(TypeActionResult {
        action: "type".to_owned(),
        text: text.to_owned(),
        char_count,
    })
}

/// Press + release the key at `slot` in the keyboard's keymap, driving Shift if
/// the placement requires it.
fn tap(kb: &Keyboard, slot: usize) {
    let p = kb.keymap.placements[slot];
    if p.needs_shift {
        kb.key(kb.keymap.shift_code, true);
    }
    kb.key(p.evdev_code, true);
    kb.key(p.evdev_code, false);
    if p.needs_shift {
        kb.key(kb.keymap.shift_code, false);
    }
}

/// Hold `keys` down for `duration_ms`, then release.
///
/// Each entry is a full CU key token (modifier name or base key). Modifiers are
/// pressed first (in given order); all keys are released in reverse order.
pub fn hold_key(conn: &Conn, keys: &[String], duration_ms: u64) -> Result<KeyboardActionResult> {
    if keys.is_empty() {
        bail!("hold key list is empty");
    }

    // Partition into modifiers (pressed first) and non-modifier base keys.
    let mut mods: Vec<&String> = Vec::new();
    let mut bases: Vec<&String> = Vec::new();
    for key in keys {
        if keymap::is_modifier(key) {
            mods.push(key);
        } else {
            bases.push(key);
        }
    }

    // Resolve base keysyms and build a keymap holding them + the modifiers.
    let base_keysyms: Vec<u32> = bases
        .iter()
        .map(|k| keymap::keysym_for_name(k))
        .collect::<Result<_>>()?;

    let mut queue = conn.conn.new_event_queue::<KeyboardState>();
    let qh = queue.handle();
    let kb = Keyboard::create(conn, &qh, &base_keysyms)?;

    let mod_codes: Vec<u32> = mods
        .iter()
        .map(|m| keymap::modifier_evdev_code(&kb.keymap, m))
        .collect::<Result<_>>()?;

    // Press: modifiers, then each base key (with its shift level if needed).
    for &code in &mod_codes {
        kb.key(code, true);
    }
    let any_shift = kb.keymap.placements.iter().any(|p| p.needs_shift);
    if any_shift {
        kb.key(kb.keymap.shift_code, true);
    }
    for p in &kb.keymap.placements {
        kb.key(p.evdev_code, true);
    }
    queue.flush().ok();

    thread::sleep(Duration::from_millis(duration_ms));

    // Release base keys (reverse), shift, then modifiers (reverse).
    for p in kb.keymap.placements.iter().rev() {
        kb.key(p.evdev_code, false);
    }
    if any_shift {
        kb.key(kb.keymap.shift_code, false);
    }
    for &code in mod_codes.iter().rev() {
        kb.key(code, false);
    }
    queue
        .roundtrip(&mut KeyboardState)
        .context("hold-key roundtrip")?;

    Ok(KeyboardActionResult {
        action: "hold-key".to_owned(),
        keys: keys.to_vec(),
        repeat: None,
        duration_ms: Some(duration_ms),
    })
}

/// A set of modifier keys held on a virtual keyboard for the lifetime of the
/// value, for chording pointer clicks (Ctrl+click). Released on drop.
pub struct ModifierHold {
    kb: Keyboard,
    codes: Vec<u32>,
    queue: wayland_client::EventQueue<KeyboardState>,
}

impl ModifierHold {
    /// Press the given modifier names and keep them held.
    pub fn press(conn: &Conn, modifiers: &[String]) -> Result<Self> {
        let mut queue = conn.conn.new_event_queue::<KeyboardState>();
        let qh = queue.handle();
        // A keymap with no base keys still binds the four modifier keys.
        let kb = Keyboard::create(conn, &qh, &[])?;
        let mut codes = Vec::with_capacity(modifiers.len());
        for name in modifiers {
            codes.push(keymap::modifier_evdev_code(&kb.keymap, name)?);
        }
        for &code in &codes {
            kb.key(code, true);
        }
        queue
            .roundtrip(&mut KeyboardState)
            .context("modifier press roundtrip")?;
        Ok(Self { kb, codes, queue })
    }
}

impl Drop for ModifierHold {
    fn drop(&mut self) {
        for &code in self.codes.iter().rev() {
            self.kb.key(code, false);
        }
        // Best-effort release before the connection closes.
        let _ = self.queue.roundtrip(&mut KeyboardState);
    }
}

#[cfg(test)]
mod tests {
    // The keyboard paths require a live compositor (virtual keyboard + seat),
    // so behavioral coverage lives in the headless-sway smoke test. The keymap
    // generation these functions depend on is unit-tested in `keymap.rs`.
    // Here we only assert the module compiles and constants are sane.
    use super::*;

    #[test]
    fn key_state_constants() {
        assert_eq!(KEY_PRESSED, 1);
        assert_eq!(KEY_RELEASED, 0);
        assert_eq!(XKB_V1, 1);
    }
}
