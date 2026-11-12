//! CU key-spec parsing and XKB keymap-text generation for the virtual keyboard.
//!
//! Two responsibilities:
//!
//! 1. **Parse** CU key specs like `ctrl+shift+tab` into modifiers + a base key,
//!    and map CU key-names / literal chars to X11 keysym values. The grammar
//!    and the CU_KEY_NAMES / MODIFIER_TOKENS tables are ported verbatim from
//!    x11-bridge's `keymap.rs` (which extracted them from `_mapKey` in
//!    `cu_linux_executor.js`), so the two bridges accept identical specs.
//!
//! 2. **Generate** an XKB keymap *text* that binds a sequence of keysyms to
//!    sequential keycodes, plus the modifier keys, WITHOUT libxkbcommon. The
//!    Wayland `zwp_virtual_keyboard_v1` protocol requires the client to upload
//!    a keymap fd; the compositor compiles the text itself. We hand-roll the
//!    text (the wtype technique) so the binary stays pure-Rust / static-musl:
//!    the client never links xkbcommon.
//!
//! Key-spec grammar (matches the JS `combo.split("+")`):
//!   spec        := token ("+" token)*
//!   token       := modifier | keyname
//!   modifier    := "ctrl" | "control" | "alt" | "shift"
//!                | "super" | "meta" | "cmd" | "command"   (cmd/meta/command -> super)
//!   keyname     := one of the CU_KEY_NAMES entries, OR a single literal char.
//! Matching is case-insensitive; the base key is the last non-modifier token.

use anyhow::{Result, bail};
use xkeysym::Keysym as XKeysym;

/// A parsed key spec: modifier keysym-names plus the base key name.
#[derive(Debug, Clone)]
pub struct KeySpec {
    pub modifiers: Vec<String>,
    pub base: String,
}

/// Modifier tokens (lowercased) recognized at the front of a key spec.
/// `cmd`/`command`/`meta` normalize to `super`.
pub const MODIFIER_TOKENS: &[&str] = &[
    "ctrl", "control", "alt", "shift", "super", "meta", "cmd", "command",
];

/// CU key-name -> X11 keysym-name table, extracted from `_mapKey`.
/// Underscore forms (`page_up`, `page_down`) are included as aliases; the
/// parser also accepts a caller stripping `_` before lookup.
pub const CU_KEY_NAMES: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("return", "Return"),
    ("backspace", "BackSpace"),
    ("delete", "Delete"),
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("tab", "Tab"),
    ("space", "space"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Prior"),
    ("page_up", "Prior"),
    ("pagedown", "Next"),
    ("page_down", "Next"),
    ("capslock", "Caps_Lock"),
];

/// X11 keysym-name -> keysym value table.
///
/// Covers the CU table's target names plus the modifier keysyms and the common
/// extra names a raw X keysym-name passthrough might carry (function keys,
/// navigation). Names are matched case-sensitively (X keysym names are, e.g.
/// `Return` vs `return`).
const KEYSYM_NAMES: &[(&str, u32)] = &[
    // Modifiers (resolved as base keys too, e.g. hold-key --key ctrl).
    ("Control_L", xkeysym::key::Control_L),
    ("Control_R", xkeysym::key::Control_R),
    ("Shift_L", xkeysym::key::Shift_L),
    ("Shift_R", xkeysym::key::Shift_R),
    ("Alt_L", xkeysym::key::Alt_L),
    ("Alt_R", xkeysym::key::Alt_R),
    ("Super_L", xkeysym::key::Super_L),
    ("Super_R", xkeysym::key::Super_R),
    ("Meta_L", xkeysym::key::Meta_L),
    ("Meta_R", xkeysym::key::Meta_R),
    ("Caps_Lock", xkeysym::key::Caps_Lock),
    ("Num_Lock", xkeysym::key::Num_Lock),
    // CU named keys.
    ("Return", xkeysym::key::Return),
    ("BackSpace", xkeysym::key::BackSpace),
    ("Delete", xkeysym::key::Delete),
    ("Escape", xkeysym::key::Escape),
    ("Tab", xkeysym::key::Tab),
    ("space", xkeysym::key::space),
    ("Up", xkeysym::key::Up),
    ("Down", xkeysym::key::Down),
    ("Left", xkeysym::key::Left),
    ("Right", xkeysym::key::Right),
    ("Home", xkeysym::key::Home),
    ("End", xkeysym::key::End),
    ("Prior", xkeysym::key::Prior),
    ("Next", xkeysym::key::Next),
    ("Insert", xkeysym::key::Insert),
    ("Menu", xkeysym::key::Menu),
    ("Print", xkeysym::key::Print),
    ("Pause", xkeysym::key::Pause),
    // Function keys F1..F24 (common raw passthrough names).
    ("F1", xkeysym::key::F1),
    ("F2", xkeysym::key::F2),
    ("F3", xkeysym::key::F3),
    ("F4", xkeysym::key::F4),
    ("F5", xkeysym::key::F5),
    ("F6", xkeysym::key::F6),
    ("F7", xkeysym::key::F7),
    ("F8", xkeysym::key::F8),
    ("F9", xkeysym::key::F9),
    ("F10", xkeysym::key::F10),
    ("F11", xkeysym::key::F11),
    ("F12", xkeysym::key::F12),
    ("F13", xkeysym::key::F13),
    ("F14", xkeysym::key::F14),
    ("F15", xkeysym::key::F15),
    ("F16", xkeysym::key::F16),
    ("F17", xkeysym::key::F17),
    ("F18", xkeysym::key::F18),
    ("F19", xkeysym::key::F19),
    ("F20", xkeysym::key::F20),
    ("F21", xkeysym::key::F21),
    ("F22", xkeysym::key::F22),
    ("F23", xkeysym::key::F23),
    ("F24", xkeysym::key::F24),
];

/// Normalize a token for CU-table / modifier lookup: trim, lowercase, and drop
/// underscores (so `page_down` == `pagedown`).
fn normalize_token(token: &str) -> String {
    token
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|&c| c != '_')
        .collect()
}

/// True if `token` names a modifier. `cmd`/`command`/`meta`/`super` all count.
pub fn is_modifier(token: &str) -> bool {
    MODIFIER_TOKENS.contains(&normalize_token(token).as_str())
}

/// Canonical X11 keysym-name for a modifier token (`ctrl` -> `Control_L`, ...).
fn modifier_keysym_name(token: &str) -> Option<&'static str> {
    match normalize_token(token).as_str() {
        "ctrl" | "control" => Some("Control_L"),
        "alt" => Some("Alt_L"),
        "shift" => Some("Shift_L"),
        "super" | "meta" | "cmd" | "command" => Some("Super_L"),
        _ => None,
    }
}

/// Parse a CU key spec (`ctrl+shift+tab`) into modifiers + base key.
///
/// Tokens split on `+`, trimmed, empty dropped. All-but-last modifier tokens
/// become modifiers; the last non-modifier token is the base. A spec that is
/// *all* modifiers (e.g. `ctrl`) uses its last token as the base.
pub fn parse_spec(spec: &str) -> Result<KeySpec> {
    let tokens: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();

    if tokens.is_empty() {
        bail!("key spec is empty");
    }

    let base_idx = tokens
        .iter()
        .rposition(|token| !is_modifier(token))
        .unwrap_or(tokens.len() - 1);

    let mut modifiers = Vec::new();
    for (idx, token) in tokens.iter().enumerate() {
        if idx == base_idx {
            continue;
        }
        if !is_modifier(token) {
            bail!("unexpected non-modifier token `{token}` before base key in `{spec}`");
        }
        modifiers.push((*token).to_owned());
    }

    Ok(KeySpec {
        modifiers,
        base: tokens[base_idx].to_owned(),
    })
}

/// Resolve a CU key-name (or literal char) to its X11 keysym-name.
///
/// Mirrors the JS `_mapKey`: known CU tokens map to their keysym name;
/// modifiers map to their `*_L` keysym name; single characters pass through;
/// anything else is treated as a raw X11 keysym name.
pub fn keysym_name(name: &str) -> Result<String> {
    if let Some(modifier) = modifier_keysym_name(name) {
        return Ok(modifier.to_owned());
    }

    let normalized = normalize_token(name);
    if let Some((_, keysym_name)) = CU_KEY_NAMES.iter().find(|(cu, _)| *cu == normalized) {
        return Ok((*keysym_name).to_owned());
    }

    let trimmed = name.trim();
    if trimmed.chars().count() == 1 {
        return Ok(trimmed.to_owned());
    }

    Ok(trimmed.to_owned())
}

/// Resolve a key-spec token (CU name / modifier / literal char / raw keysym
/// name) to an X11 keysym *value*.
pub fn keysym_for_name(name: &str) -> Result<u32> {
    let resolved = keysym_name(name)?;

    if let Some((_, value)) = KEYSYM_NAMES.iter().find(|(n, _)| *n == resolved) {
        return Ok(*value);
    }

    if resolved.chars().count() == 1 {
        let ch = resolved.chars().next().expect("count == 1");
        return keysym_for_char(ch);
    }

    if let Some(value) = keysym_from_x_name(&resolved) {
        return Ok(value);
    }

    bail!("unsupported key name `{name}` (resolved keysym name `{resolved}`)")
}

/// Look a keysym up by its X name (`comma`, `bracketleft`, `asciitilde`,
/// `eacute`, an optional `XK_` prefix), the names xdotool takes. An exact
/// match wins; otherwise the first case-insensitive one (`COMMA` -> `comma`),
/// so `Eacute` and `eacute` stay distinct. Scans the named keysym ranges
/// (Latin-1 through the 0xffxx function block, plus the XF86 block); this runs
/// only for names the tables above do not know.
fn keysym_from_x_name(name: &str) -> Option<u32> {
    let name = name.strip_prefix("XK_").unwrap_or(name);
    let mut folded = None;
    for raw in (0x0020..=0xffff_u32).chain(0x1008_ff00..=0x1008_ffff) {
        let Some(x_name) = XKeysym::new(raw).name() else {
            continue;
        };
        let x_name = x_name.strip_prefix("XK_").unwrap_or(x_name);
        if x_name == name {
            return Some(raw);
        }
        if folded.is_none() && x_name.eq_ignore_ascii_case(name) {
            folded = Some(raw);
        }
    }
    folded
}

/// Map a Unicode scalar to an X11 keysym.
///
/// Basic-Latin / Latin-1 chars have keysym == codepoint. Everything else uses
/// the Unicode keysym encoding `0x01000000 | codepoint` (the wtype technique),
/// which every XKB-based compositor accepts. `\n`/`\r` -> Return, `\t` -> Tab.
pub fn keysym_for_char(ch: char) -> Result<u32> {
    match ch {
        '\n' | '\r' => return Ok(xkeysym::key::Return),
        '\t' => return Ok(xkeysym::key::Tab),
        _ => {}
    }

    // Prefer xkeysym's Latin-1 / named mapping when it resolves (e.g. 'é' ->
    // eacute 0xe9, 'a' -> 0x61), so we produce the canonical keysym a real
    // keymap would carry.
    let ks = XKeysym::from_char(ch);
    if ks != XKeysym::NoSymbol {
        return Ok(ks.raw());
    }

    // Fall back to the Unicode encoding for anything without a named keysym
    // (CJK, emoji, ...). The compositor's xkbcommon resolves these via the
    // `U0000`-style symbol names our keymap text emits.
    let cp = ch as u32;
    if cp == 0 {
        bail!("cannot type NUL");
    }
    Ok(0x0100_0000 | cp)
}

/// The XKB symbol-name for a keysym, as it must appear inside an
/// `xkb_symbols` section (e.g. `Return`, `a`, `U00E9`, `U1F600`).
///
/// - Known names from `KEYSYM_NAMES` use their canonical X name.
/// - Unicode-encoded keysyms (`0x01000000 | cp`) use `U%04X` (xkbcommon's
///   Unicode symbol syntax).
/// - Latin-1 keysyms fall back to xkeysym's name if it has one, else `U%04X`
///   of the equivalent codepoint.
fn xkb_symbol_name(keysym: u32) -> String {
    if let Some((name, _)) = KEYSYM_NAMES.iter().find(|(_, v)| *v == keysym) {
        return (*name).to_owned();
    }
    if keysym & 0xff00_0000 == 0x0100_0000 {
        let cp = keysym & 0x00ff_ffff;
        return format!("U{cp:04X}");
    }
    // Named Latin-1 / basic keysym: ask xkeysym for its X name.
    if let Some(name) = XKeysym::new(keysym).name() {
        // xkeysym returns e.g. "XK_a"; strip the "XK_" prefix xkb does not use.
        return name.strip_prefix("XK_").unwrap_or(name).to_owned();
    }
    // Last resort: treat the value as a codepoint (true for basic Latin).
    format!("U{keysym:04X}")
}

/// Position of a key within a generated keymap: its evdev-style keycode (as the
/// virtual-keyboard `key` request wants it, i.e. XKB keycode - 8) and whether
/// Shift must be held to produce its keysym (it sits at level 2).
#[derive(Debug, Clone, Copy)]
pub struct KeyPlacement {
    /// The keycode to pass to `zwp_virtual_keyboard_v1.key` (Linux evdev code =
    /// XKB keycode - 8).
    pub evdev_code: u32,
    pub needs_shift: bool,
}

/// A generated XKB keymap: the text to upload, plus the placement of each
/// requested keysym and the modifier keys.
#[derive(Debug, Clone)]
pub struct GeneratedKeymap {
    /// The full `xkb_keymap { ... }` text, NUL-free (caller appends the NUL).
    pub text: String,
    /// evdev code for each modifier we place: ctrl, shift, alt, super.
    pub ctrl_code: u32,
    pub shift_code: u32,
    pub alt_code: u32,
    pub super_code: u32,
    /// Placement for each requested base keysym, in the order requested.
    pub placements: Vec<KeyPlacement>,
}

/// Where keys sit in a generated keymap.
///
/// Compositors do not all match their own key bindings against the keymap a
/// virtual keyboard uploads: Hyprland (seen on 0.56) resolves a virtual
/// keyboard's key by its evdev code in the user's own layout. A key placed at
/// an arbitrary code then triggers whatever the user's layout has there (with
/// the old sequential layout, Control sat on evdev 1 = Escape, so every
/// injected Ctrl fired Escape binds). So every keysym that has a position in
/// the US pc105 layout goes to that position (Escape on evdev 1, Control_L 29,
/// Shift_L 42, Alt_L 56, Super_L 125, letters, digits, punctuation, named
/// keys); everything else goes to codes the US layout leaves unmapped, then to
/// the alphanumeric block, then to the rest, never to xkb 9 (Escape) or a code
/// that carries a modifier or lock in the US layout. Applications are not
/// affected: they follow the uploaded keymap, where every placed code carries
/// exactly the requested keysym.
///
/// `type` spills more narrowly, to [`layout::printable`] codes only: clients
/// do look at the code. Chromium names a key by its code and decides from that
/// name whether it is a character key at all, so a character on an unmapped
/// code never arrives and one on Backspace deletes instead.
pub mod layout {
    /// XKB keycodes of the four modifiers we drive (evdev = xkb - 8).
    pub const CTRL: u32 = 37;
    pub const SHIFT: u32 = 50;
    pub const ALT: u32 = 64;
    pub const SUPER: u32 = 133;
    /// XKB keycode of Escape (evdev 1, KEY_ESC).
    pub const ESCAPE: u32 = 9;
    /// Highest keycode used (X11 clients via Xwayland cannot see more).
    pub const MAX: u32 = 255;

    /// US pc105 (evdev rules) positions: (xkb keycode, level 1, level 2 or 0),
    /// from `xkbcli compile-keymap --rules evdev --model pc105 --layout us`.
    pub const US: &[(u32, u32, u32)] = &[
        (9, 0xff1b, 0),       // Escape
        (10, 0x31, 0x21),     // 1 !
        (11, 0x32, 0x40),     // 2 @
        (12, 0x33, 0x23),     // 3 #
        (13, 0x34, 0x24),     // 4 $
        (14, 0x35, 0x25),     // 5 %
        (15, 0x36, 0x5e),     // 6 ^
        (16, 0x37, 0x26),     // 7 &
        (17, 0x38, 0x2a),     // 8 *
        (18, 0x39, 0x28),     // 9 (
        (19, 0x30, 0x29),     // 0 )
        (20, 0x2d, 0x5f),     // - _
        (21, 0x3d, 0x2b),     // = +
        (22, 0xff08, 0),      // BackSpace
        (23, 0xff09, 0xfe20), // Tab ISO_Left_Tab
        (24, 0x71, 0x51),     // q Q
        (25, 0x77, 0x57),     // w W
        (26, 0x65, 0x45),     // e E
        (27, 0x72, 0x52),     // r R
        (28, 0x74, 0x54),     // t T
        (29, 0x79, 0x59),     // y Y
        (30, 0x75, 0x55),     // u U
        (31, 0x69, 0x49),     // i I
        (32, 0x6f, 0x4f),     // o O
        (33, 0x70, 0x50),     // p P
        (34, 0x5b, 0x7b),     // [ {
        (35, 0x5d, 0x7d),     // ] }
        (36, 0xff0d, 0),      // Return
        (37, 0xffe3, 0),      // Control_L
        (38, 0x61, 0x41),     // a A
        (39, 0x73, 0x53),     // s S
        (40, 0x64, 0x44),     // d D
        (41, 0x66, 0x46),     // f F
        (42, 0x67, 0x47),     // g G
        (43, 0x68, 0x48),     // h H
        (44, 0x6a, 0x4a),     // j J
        (45, 0x6b, 0x4b),     // k K
        (46, 0x6c, 0x4c),     // l L
        (47, 0x3b, 0x3a),     // ; :
        (48, 0x27, 0x22),     // ' "
        (49, 0x60, 0x7e),     // ` ~
        (50, 0xffe1, 0),      // Shift_L
        (51, 0x5c, 0x7c),     // \ |
        (52, 0x7a, 0x5a),     // z Z
        (53, 0x78, 0x58),     // x X
        (54, 0x63, 0x43),     // c C
        (55, 0x76, 0x56),     // v V
        (56, 0x62, 0x42),     // b B
        (57, 0x6e, 0x4e),     // n N
        (58, 0x6d, 0x4d),     // m M
        (59, 0x2c, 0x3c),     // , <
        (60, 0x2e, 0x3e),     // . >
        (61, 0x2f, 0x3f),     // / ?
        (62, 0xffe2, 0),      // Shift_R
        (64, 0xffe9, 0),      // Alt_L
        (65, 0x20, 0),        // space
        (66, 0xffe5, 0),      // Caps_Lock
        (67, 0xffbe, 0),      // F1
        (68, 0xffbf, 0),      // F2
        (69, 0xffc0, 0),      // F3
        (70, 0xffc1, 0),      // F4
        (71, 0xffc2, 0),      // F5
        (72, 0xffc3, 0),      // F6
        (73, 0xffc4, 0),      // F7
        (74, 0xffc5, 0),      // F8
        (75, 0xffc6, 0),      // F9
        (76, 0xffc7, 0),      // F10
        (95, 0xffc8, 0),      // F11
        (96, 0xffc9, 0),      // F12
        (105, 0xffe4, 0),     // Control_R
        (108, 0xffea, 0),     // Alt_R
        (110, 0xff50, 0),     // Home
        (111, 0xff52, 0),     // Up
        (112, 0xff55, 0),     // Prior
        (113, 0xff51, 0),     // Left
        (114, 0xff53, 0),     // Right
        (115, 0xff57, 0),     // End
        (116, 0xff54, 0),     // Down
        (117, 0xff56, 0),     // Next
        (118, 0xff63, 0),     // Insert
        (119, 0xffff, 0),     // Delete
        (133, 0xffeb, 0),     // Super_L
        (134, 0xffec, 0),     // Super_R
    ];

    /// Codes carrying a modifier or lock in the US layout (its modifier_map):
    /// only ever used for exactly that keysym.
    pub const US_MODIFIERS: &[u32] = &[
        37, 50, 62, 64, 66, 77, 92, 105, 108, 133, 134, 203, 204, 205, 206,
    ];

    /// Codes (<= 255) the US layout leaves without any symbol. The two that
    /// Chromium (and so every Electron app) maps to a key, IntlRo and IntlYen,
    /// come first: Chromium drops a key whose code it has no name for.
    pub const US_UNMAPPED: &[u32] = &[
        97, 132, 103, 120, 149, 154, 168, 178, 183, 184, 219, 222, 230, 248,
    ];

    /// Codes a client takes for an ordinary character key whatever symbol the
    /// keymap puts there: the alphanumeric block plus IntlBackslash (94),
    /// IntlRo (97) and IntlYen (132). Chromium decides from the code alone
    /// what else a key does, so a character placed on any other code is lost
    /// or does that key's job instead: nothing at all for codes it has no
    /// name for (KEY_KPJPCOMMA and up), deleting on Backspace, and opening
    /// Help on F1. `type` places characters only here (or on their own named
    /// key: space, Return, Tab).
    pub fn printable() -> impl Iterator<Item = u32> + Clone {
        [97, 132, 94]
            .into_iter()
            .chain(10..=21)
            .chain(24..=35)
            .chain(38..=49)
            .chain(51..=61)
    }

    /// The US position of `keysym`: (code, at level 1).
    pub fn standard(keysym: u32) -> Option<(u32, bool)> {
        US.iter().find_map(|&(kc, l1, l2)| {
            if l1 == keysym {
                Some((kc, true))
            } else if l2 != 0 && l2 == keysym {
                Some((kc, false))
            } else {
                None
            }
        })
    }

    /// Codes for keysyms without a free US position, best first.
    pub fn spill() -> impl Iterator<Item = u32> {
        US_UNMAPPED
            .iter()
            .copied()
            .chain(10..=61)
            .chain(62..=MAX)
            .filter(|kc| *kc != ESCAPE && !US_MODIFIERS.contains(kc))
    }
}

/// Whether a modifier token is Shift.
pub fn is_shift(token: &str) -> bool {
    modifier_keysym_name(token) == Some("Shift_L")
}

/// [`generate_keymap_with`] without real shift levels: every keysym sits at
/// both levels of its key, so it arrives exactly as requested even with a
/// stray Shift (`type`, and combos without Shift).
#[cfg(test)]
pub fn generate_keymap(keysyms: &[u32]) -> GeneratedKeymap {
    generate_keymap_with(keysyms, false)
}

/// Build an XKB keymap text binding the four modifier keys plus every keysym
/// in `keysyms` (see [`layout`] for where each one goes). A keysym requested
/// twice gets the same keycode.
///
/// For `key-sequence` / `hold-key`; `type` uses [`type_pages`].
///
/// Shift levels: without `shift_levels` each requested keysym sits at both
/// levels of its key (`[ a, a ]`), so it arrives exactly as requested even
/// with Shift held. With `shift_levels` (a `key-sequence` / `hold-key` combo
/// that holds Shift), a key placed at its US position carries the US pair
/// (`[ a, A ]`, `[ 1, exclam ]`, `[ Tab, ISO_Left_Tab ]`), so Shift selects
/// the shifted symbol as on a real keyboard (`shift+a` -> `A`); a requested
/// keysym that IS the shifted symbol (`shift+A`) still arrives exactly.
/// Spilled keys (no US position) keep both levels equal.
///
/// The `key` request wants the evdev code (= XKB keycode - 8), so the returned
/// placements carry `xkb_keycode - 8`.
pub fn generate_keymap_with(keysyms: &[u32], shift_levels: bool) -> GeneratedKeymap {
    build_keymap(keysyms, shift_levels, layout::spill(), true).expect("overflow codes always fit")
}

/// A keymap for `type`: like [`generate_keymap`], but keysyms without a free
/// US position go only to [`layout::printable`] codes. `None` when they do not
/// all fit (50 characters, plus space, Return and Tab on their own keys).
pub fn generate_type_keymap(keysyms: &[u32]) -> Option<GeneratedKeymap> {
    build_keymap(keysyms, false, layout::printable(), false)
}

/// One keymap of a `type` run and the characters typed with it.
#[derive(Debug, Clone)]
pub struct TypePage {
    pub keymap: GeneratedKeymap,
    /// For each character of the page, in order: its placement index.
    pub slots: Vec<usize>,
}

/// Split the keysyms of a text into consecutive pages that each fit one
/// [`generate_type_keymap`] keymap. Most texts need one page; a text with
/// more distinct characters than printable keys gets a new keymap uploaded
/// where the next character no longer fits.
pub fn type_pages(keysyms: &[u32]) -> Vec<TypePage> {
    let mut pages = Vec::new();
    let mut distinct: Vec<u32> = Vec::new();
    let mut slots: Vec<usize> = Vec::new();
    let mut current: Option<GeneratedKeymap> = None;
    for &ks in keysyms {
        if let Some(i) = distinct.iter().position(|&k| k == ks) {
            slots.push(i);
            continue;
        }
        distinct.push(ks);
        if let Some(km) = generate_type_keymap(&distinct) {
            current = Some(km);
            slots.push(distinct.len() - 1);
            continue;
        }
        // Does not fit: close the page and start the next with this key.
        if let Some(keymap) = current.take() {
            pages.push(TypePage {
                keymap,
                slots: std::mem::take(&mut slots),
            });
        }
        distinct = vec![ks];
        current = Some(generate_type_keymap(&distinct).expect("one key always fits"));
        slots.push(0);
    }
    if let Some(keymap) = current {
        pages.push(TypePage { keymap, slots });
    }
    pages
}

/// The keymap builder behind [`generate_keymap_with`] and
/// [`generate_type_keymap`]: keysyms without a free US position take codes
/// from `spill`, then (with `overflow`) codes past [`layout::MAX`]; without
/// `overflow`, `None` once `spill` is used up.
fn build_keymap(
    keysyms: &[u32],
    shift_levels: bool,
    spill: impl Iterator<Item = u32>,
    overflow_ok: bool,
) -> Option<GeneratedKeymap> {
    use std::collections::{BTreeMap, HashMap};

    // keycode -> keysym bound there.
    let mut used: BTreeMap<u32, u32> = BTreeMap::new();
    used.insert(layout::CTRL, xkeysym::key::Control_L);
    used.insert(layout::SHIFT, xkeysym::key::Shift_L);
    used.insert(layout::ALT, xkeysym::key::Alt_L);
    used.insert(layout::SUPER, xkeysym::key::Super_L);
    let mut code_of: HashMap<u32, u32> = used.iter().map(|(kc, ks)| (*ks, *kc)).collect();

    // Pass 1: exact level-1 positions; pass 2: level-2 positions.
    for level1 in [true, false] {
        for &ks in keysyms {
            if code_of.contains_key(&ks) {
                continue;
            }
            if let Some((kc, l1)) = layout::standard(ks)
                && l1 == level1
                && !used.contains_key(&kc)
            {
                used.insert(kc, ks);
                code_of.insert(ks, kc);
            }
        }
    }
    // Pass 3: everything else into the spill order (past 255 only if a
    // combo ever asks for more keys than fit; `type` pages instead).
    let mut spill = spill;
    let mut overflow = layout::MAX + 1;
    for &ks in keysyms {
        if code_of.contains_key(&ks) {
            continue;
        }
        let kc = loop {
            match spill.next() {
                Some(kc) if !used.contains_key(&kc) => break kc,
                Some(_) => continue,
                None if overflow_ok => {
                    overflow += 1;
                    break overflow - 1;
                }
                None => return None,
            }
        };
        used.insert(kc, ks);
        code_of.insert(ks, kc);
    }

    let max_kc = used.keys().next_back().copied().unwrap_or(layout::SUPER);

    // --- xkb_keycodes: declare <Kxx> for every keycode we use. ---
    let mut keycodes = String::new();
    keycodes.push_str("xkb_keycodes \"wlroots-bridge\" {\n");
    keycodes.push_str("  minimum = 8;\n");
    keycodes.push_str(&format!("  maximum = {};\n", max_kc.max(layout::MAX)));
    for kc in used.keys() {
        keycodes.push_str(&format!("  <K{kc}> = {kc};\n"));
    }
    keycodes.push_str("};\n");

    // --- xkb_types: use the stock "complete" types (compositor ships them). ---
    let types = "xkb_types \"wlroots-bridge\" { include \"complete\" };\n";

    // --- xkb_compat: stock rules so modifiers behave. ---
    let compat = "xkb_compat \"wlroots-bridge\" { include \"complete\" };\n";

    // --- xkb_symbols: every placed key, and the modifier_map. ---
    let mut symbols = String::new();
    symbols.push_str("xkb_symbols \"wlroots-bridge\" {\n");
    for (kc, ks) in &used {
        let sym = xkb_symbol_name(*ks);
        if [layout::CTRL, layout::SHIFT, layout::ALT, layout::SUPER].contains(kc) {
            symbols.push_str(&format!("  key <K{kc}> {{ [ {sym} ] }};\n"));
            continue;
        }
        // The US pair when this key sits at its own US position and has a
        // shifted symbol there.
        let pair = layout::US
            .iter()
            .find(|(c, l1, l2)| c == kc && *l2 != 0 && (*l1 == *ks || *l2 == *ks));
        match pair {
            Some(&(_, l1, l2)) if shift_levels => symbols.push_str(&format!(
                "  key <K{kc}> {{ [ {}, {} ] }};\n",
                xkb_symbol_name(l1),
                xkb_symbol_name(l2)
            )),
            _ => symbols.push_str(&format!("  key <K{kc}> {{ [ {sym}, {sym} ] }};\n")),
        }
    }
    let placements = keysyms
        .iter()
        .map(|ks| KeyPlacement {
            evdev_code: code_of[ks] - 8,
            // The exact keysym is bound at level 1, so Shift is never needed.
            needs_shift: false,
        })
        .collect();

    symbols.push_str(&format!(
        "  modifier_map Control {{ <K{}> }};\n",
        layout::CTRL
    ));
    symbols.push_str(&format!(
        "  modifier_map Shift {{ <K{}> }};\n",
        layout::SHIFT
    ));
    symbols.push_str(&format!("  modifier_map Mod1 {{ <K{}> }};\n", layout::ALT));
    symbols.push_str(&format!(
        "  modifier_map Mod4 {{ <K{}> }};\n",
        layout::SUPER
    ));
    symbols.push_str("};\n");

    let text = format!("xkb_keymap {{\n{keycodes}{types}{compat}{symbols}}};\n");

    Some(GeneratedKeymap {
        text,
        ctrl_code: layout::CTRL - 8,
        shift_code: layout::SHIFT - 8,
        alt_code: layout::ALT - 8,
        super_code: layout::SUPER - 8,
        placements,
    })
}

/// The evdev code for a modifier token, within a generated keymap.
pub fn modifier_evdev_code(keymap: &GeneratedKeymap, token: &str) -> Result<u32> {
    match modifier_keysym_name(token) {
        Some("Control_L") => Ok(keymap.ctrl_code),
        Some("Shift_L") => Ok(keymap.shift_code),
        Some("Alt_L") => Ok(keymap.alt_code),
        Some("Super_L") => Ok(keymap.super_code),
        _ => bail!("`{token}` is not a modifier"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifier_chord() {
        let spec = parse_spec("ctrl+shift+tab").unwrap();
        assert_eq!(spec.modifiers, vec!["ctrl", "shift"]);
        assert_eq!(spec.base, "tab");
    }

    #[test]
    fn parses_cmd_as_super_base_last() {
        let spec = parse_spec("cmd+a").unwrap();
        assert_eq!(spec.modifiers, vec!["cmd"]);
        assert_eq!(spec.base, "a");
    }

    #[test]
    fn parses_single_key() {
        let spec = parse_spec("return").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "return");
    }

    #[test]
    fn parses_bare_modifier_as_base() {
        let spec = parse_spec("ctrl").unwrap();
        assert!(spec.modifiers.is_empty());
        assert_eq!(spec.base, "ctrl");
    }

    #[test]
    fn empty_spec_errors() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("+").is_err());
    }

    #[test]
    fn keysym_names_match_cu_table() {
        assert_eq!(keysym_name("return").unwrap(), "Return");
        assert_eq!(keysym_name("Enter").unwrap(), "Return");
        assert_eq!(keysym_name("escape").unwrap(), "Escape");
        assert_eq!(keysym_name("esc").unwrap(), "Escape");
        assert_eq!(keysym_name("page_down").unwrap(), "Next");
        assert_eq!(keysym_name("pagedown").unwrap(), "Next");
        assert_eq!(keysym_name("page_up").unwrap(), "Prior");
        assert_eq!(keysym_name("space").unwrap(), "space");
        assert_eq!(keysym_name("capslock").unwrap(), "Caps_Lock");
    }

    #[test]
    fn keysym_names_map_modifiers() {
        assert_eq!(keysym_name("ctrl").unwrap(), "Control_L");
        assert_eq!(keysym_name("control").unwrap(), "Control_L");
        assert_eq!(keysym_name("cmd").unwrap(), "Super_L");
        assert_eq!(keysym_name("command").unwrap(), "Super_L");
        assert_eq!(keysym_name("meta").unwrap(), "Super_L");
        assert_eq!(keysym_name("super").unwrap(), "Super_L");
        assert_eq!(keysym_name("alt").unwrap(), "Alt_L");
        assert_eq!(keysym_name("shift").unwrap(), "Shift_L");
    }

    #[test]
    fn keysym_values_resolve() {
        assert_eq!(keysym_for_name("return").unwrap(), xkeysym::key::Return);
        assert_eq!(keysym_for_name("tab").unwrap(), xkeysym::key::Tab);
        assert_eq!(keysym_for_name("F5").unwrap(), xkeysym::key::F5);
        assert_eq!(keysym_for_name("ctrl").unwrap(), xkeysym::key::Control_L);
        assert_eq!(keysym_for_name("a").unwrap(), u32::from('a'));
        assert_eq!(keysym_for_name("A").unwrap(), u32::from('A'));
        assert_eq!(keysym_for_name("é").unwrap(), 0x0e9);
    }

    #[test]
    fn x_keysym_names_resolve() {
        let names = [
            ("comma", ','),
            ("period", '.'),
            ("slash", '/'),
            ("backslash", '\\'),
            ("semicolon", ';'),
            ("apostrophe", '\''),
            ("quotedbl", '"'),
            ("grave", '`'),
            ("minus", '-'),
            ("equal", '='),
            ("plus", '+'),
            ("bracketleft", '['),
            ("bracketright", ']'),
            ("braceleft", '{'),
            ("braceright", '}'),
            ("parenleft", '('),
            ("parenright", ')'),
            ("less", '<'),
            ("greater", '>'),
            ("question", '?'),
            ("colon", ':'),
            ("underscore", '_'),
            ("bar", '|'),
            ("asciitilde", '~'),
            ("exclam", '!'),
            ("at", '@'),
            ("numbersign", '#'),
            ("dollar", '$'),
            ("percent", '%'),
            ("asciicircum", '^'),
            ("ampersand", '&'),
            ("asterisk", '*'),
            ("space", ' '),
        ];
        for (name, ch) in names {
            let want = u32::from(ch);
            assert_eq!(keysym_for_name(name).unwrap(), want, "{name}");
            assert_eq!(
                keysym_for_name(&name.to_uppercase()).unwrap(),
                want,
                "{name} upper"
            );
            // The keymap must name it the way xkbcommon parses it back.
            assert_eq!(xkb_symbol_name(want), name);
        }
        assert_eq!(keysym_for_name("XK_comma").unwrap(), 0x2c);
        // Exact case wins where X names differ only by case.
        assert_eq!(keysym_for_name("Eacute").unwrap(), 0xc9);
        assert_eq!(keysym_for_name("eacute").unwrap(), 0xe9);
        assert!(keysym_for_name("notakeysym").is_err());
    }

    #[test]
    fn x_keysym_names_work_in_key_specs() {
        let spec = parse_spec("ctrl+comma").unwrap();
        assert_eq!(keysym_for_name(&spec.base).unwrap(), 0x2c);
        let spec = parse_spec("ctrl+bracketleft").unwrap();
        assert_eq!(keysym_for_name(&spec.base).unwrap(), 0x5b);
    }

    #[test]
    fn is_modifier_recognizes_aliases() {
        for token in [
            "ctrl", "Control", "ALT", "shift", "super", "meta", "cmd", "command",
        ] {
            assert!(is_modifier(token), "{token} should be a modifier");
        }
        assert!(!is_modifier("a"));
        assert!(!is_modifier("tab"));
    }

    #[test]
    fn char_keysyms_use_unicode_encoding_for_cjk() {
        // Latin-1 stays as codepoint.
        assert_eq!(keysym_for_char('a').unwrap(), 0x61);
        assert_eq!(keysym_for_char('é').unwrap(), 0xe9);
        // A CJK char has no named/Latin-1 keysym -> Unicode encoding.
        let ks = keysym_for_char('好').unwrap();
        assert_eq!(ks, 0x0100_0000 | u32::from('好'));
        // Emoji likewise.
        let smile = keysym_for_char('😀').unwrap();
        assert_eq!(smile, 0x0100_0000 | 0x1F600);
        // Newline / tab map to Return / Tab.
        assert_eq!(keysym_for_char('\n').unwrap(), xkeysym::key::Return);
        assert_eq!(keysym_for_char('\t').unwrap(), xkeysym::key::Tab);
    }

    #[test]
    fn xkb_symbol_names_are_well_formed() {
        assert_eq!(xkb_symbol_name(xkeysym::key::Return), "Return");
        assert_eq!(xkb_symbol_name(xkeysym::key::space), "space");
        // Unicode-encoded emoji.
        assert_eq!(xkb_symbol_name(0x0100_0000 | 0x1F600), "U1F600");
        // Latin-1 é -> its xkb name if xkeysym provides one, else U00E9.
        let s = xkb_symbol_name(0xe9);
        assert!(s == "eacute" || s == "U00E9", "got {s}");
    }

    #[test]
    fn generated_keymap_is_well_formed() {
        let syms = vec![
            keysym_for_char('h').unwrap(),
            keysym_for_char('i').unwrap(),
            keysym_for_char('好').unwrap(),
        ];
        let km = generate_keymap(&syms);

        // Structural sanity: the four sections and the wrapping braces exist.
        assert!(km.text.starts_with("xkb_keymap {"));
        assert!(km.text.contains("xkb_keycodes"));
        assert!(km.text.contains("xkb_types"));
        assert!(km.text.contains("xkb_compat"));
        assert!(km.text.contains("xkb_symbols"));
        assert!(km.text.trim_end().ends_with("};"));
        // No NUL sneaks into the text (we append it separately when writing).
        assert!(!km.text.contains('\0'));
        // Balanced braces.
        let opens = km.text.matches('{').count();
        let closes = km.text.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in keymap");

        // One placement per requested keysym, at their US positions.
        assert_eq!(km.placements.len(), 3);
        assert_eq!(km.placements[0].evdev_code, 35); // h = KEY_H
        assert_eq!(km.placements[1].evdev_code, 23); // i = KEY_I
        // No US position: the first unmapped code.
        assert_eq!(km.placements[2].evdev_code, layout::US_UNMAPPED[0] - 8);

        // The CJK char is placed as a Unicode symbol.
        assert!(km.text.contains("U597D")); // 好 = U+597D
        // modifier_map entries present.
        assert!(km.text.contains("modifier_map Control"));
        assert!(km.text.contains("modifier_map Shift"));
    }

    #[test]
    fn generated_keymap_handles_repeated_chars() {
        // A repeated keysym reuses its keycode; one placement per input.
        let syms: Vec<u32> = "aaa".chars().map(|c| keysym_for_char(c).unwrap()).collect();
        let km = generate_keymap(&syms);
        assert_eq!(km.placements.len(), 3);
        assert!(km.placements.iter().all(|p| p.evdev_code == 30));
        let opens = km.text.matches('{').count();
        let closes = km.text.matches('}').count();
        assert_eq!(opens, closes);
    }

    /// Hyprland matches a virtual keyboard's keys by evdev code in the user's
    /// layout: Escape and the modifiers must sit at their real codes, and no
    /// other key may ever take evdev 1 (KEY_ESC) or a modifier code.
    #[test]
    fn standard_evdev_codes() {
        let esc = keysym_for_name("Escape").unwrap();
        let km = generate_keymap(&[esc]);
        assert_eq!(km.placements[0].evdev_code, 1); // KEY_ESC
        assert_eq!(km.ctrl_code, 29); // KEY_LEFTCTRL
        assert_eq!(km.shift_code, 42); // KEY_LEFTSHIFT
        assert_eq!(km.alt_code, 56); // KEY_LEFTALT
        assert_eq!(km.super_code, 125); // KEY_LEFTMETA

        // ctrl+a: Control on 29, `a` on 30 (KEY_A), nothing on 1.
        let km = generate_keymap(&[keysym_for_char('a').unwrap()]);
        assert_eq!(km.placements[0].evdev_code, 30);
        assert!(
            !km.text.contains("<K9>"),
            "nothing may sit on xkb 9 (Escape)"
        );

        // Named keys at their KEY_* codes.
        for (name, evdev) in [
            ("Return", 28),
            ("Tab", 15),
            ("BackSpace", 14),
            ("space", 57),
            ("Delete", 111),
            ("Left", 105),
            ("F5", 63),
            ("F12", 88),
        ] {
            let km = generate_keymap(&[keysym_for_name(name).unwrap()]);
            assert_eq!(km.placements[0].evdev_code, evdev, "{name}");
        }

        // A modifier keysym as a base key uses the modifier's own key.
        let km = generate_keymap(&[xkeysym::key::Shift_L]);
        assert_eq!(km.placements[0].evdev_code, km.shift_code);
    }

    #[test]
    fn shift_levels_only_on_request() {
        let a = keysym_for_char('a').unwrap();
        let one = keysym_for_char('1').unwrap();
        let tab = keysym_for_name("Tab").unwrap();
        let e = keysym_for_char('é').unwrap();
        // Default (`type`, combos without Shift): exact glyph on both levels.
        let km = generate_keymap(&[a, one, e]);
        assert!(km.text.contains("key <K38> { [ a, a ] };"), "{}", km.text);
        assert!(km.text.contains("key <K10> { [ 1, 1 ] };"));
        // With Shift in the combo: the US pairs.
        let km = generate_keymap_with(&[a], true);
        assert!(km.text.contains("key <K38> { [ a, A ] };"), "{}", km.text);
        let km = generate_keymap_with(&[one], true);
        assert!(km.text.contains("key <K10> { [ 1, exclam ] };"));
        let km = generate_keymap_with(&[tab], true);
        assert!(km.text.contains("key <K23> { [ Tab, ISO_Left_Tab ] };"));
        // Requesting the shifted glyph itself: same key, still the pair.
        let km = generate_keymap_with(&[keysym_for_char('A').unwrap()], true);
        assert!(km.text.contains("key <K38> { [ a, A ] };"));
        // Spilled keys keep both levels equal.
        let km = generate_keymap_with(&[e], true);
        assert!(
            km.text.contains("key <K97> { [ eacute, eacute ] };"),
            "{}",
            km.text
        );
        // Keys without a shifted symbol (Return, F5) are unchanged.
        let km = generate_keymap_with(&[keysym_for_name("Return").unwrap()], true);
        assert!(km.text.contains("key <K36> { [ Return, Return ] };"));
    }

    #[test]
    fn many_distinct_keys_avoid_escape_and_modifier_codes() {
        // 200 distinct glyphs (a combo never asks for that many, but the
        // overflow path must stay sound): Latin, shifted, CJK.
        let mut syms: Vec<u32> = (0x21u32..0x7f).collect(); // printable ASCII
        syms.extend(
            (0x4e00u32..)
                .take(200 - syms.len())
                .map(|cp| 0x0100_0000 | cp),
        );
        assert_eq!(syms.len(), 200);
        let km = generate_keymap(&syms);
        let codes: Vec<u32> = km.placements.iter().map(|p| p.evdev_code + 8).collect();
        let mut uniq = codes.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 200, "one keycode per distinct keysym");
        assert!(
            codes.iter().all(|kc| *kc <= layout::MAX),
            "fits X11 keycodes"
        );
        assert!(!codes.contains(&layout::ESCAPE));
        for m in layout::US_MODIFIERS {
            assert!(!codes.contains(m), "keycode {m} carries a modifier");
        }
        // Lowercase letters keep their US codes; their uppercase twins spill.
        assert_eq!(km.placements[(b'a' - 0x21) as usize].evdev_code, 30);
        assert_ne!(km.placements[(b'A' - 0x21) as usize].evdev_code, 30);
    }

    /// Every code a `type` keymap puts a character on, with the keysym there.
    fn type_codes(page: &TypePage, keysyms: &[u32]) -> Vec<(u32, u32)> {
        page.slots
            .iter()
            .map(|&slot| (page.keymap.placements[slot].evdev_code + 8, keysyms[slot]))
            .collect()
    }

    /// The live failure (Hyprland and sway, `de`, Electron): with `g n m e b c`
    /// already on their own keys, `G N M E B C +` went to codes Chromium has
    /// no name for (KEY_KPJPCOMMA and up) and never arrived. `type` must put
    /// them on character keys only.
    #[test]
    fn type_spills_only_to_printable_codes() {
        let text = "gnmebc+= GNMEBC+_{}|~\":!?*() €ßäöüÄÖÜ漢字";
        let keysyms: Vec<u32> = text.chars().map(|c| keysym_for_char(c).unwrap()).collect();
        let pages = type_pages(&keysyms);
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.slots.len(), keysyms.len());
        let printable: Vec<u32> = layout::printable().collect();
        let mut distinct = Vec::new();
        for &ks in &keysyms {
            if !distinct.contains(&ks) {
                distinct.push(ks);
            }
        }
        for (kc, ks) in type_codes(page, &distinct) {
            let own_key = layout::standard(ks).map(|(c, _)| c) == Some(kc);
            assert!(
                printable.contains(&kc) || own_key,
                "{} on code {kc}",
                xkb_symbol_name(ks)
            );
            // Exactly that keysym on both levels of the key.
            let sym = xkb_symbol_name(ks);
            assert!(
                page.keymap
                    .text
                    .contains(&format!("key <K{kc}> {{ [ {sym}, {sym} ] }};")),
                "{sym} at {kc}: {}",
                page.keymap.text
            );
        }
        // Never Backspace, Tab, Return, Escape or a function key for a spilled
        // character: Chromium would run that key's action instead.
        let spilled = type_codes(page, &distinct);
        for kc in [9, 22, 23, 36, 67, 68] {
            assert!(!spilled.iter().any(|(c, _)| *c == kc), "code {kc} used");
        }
    }

    #[test]
    fn printable_codes_are_character_keys() {
        let codes: Vec<u32> = layout::printable().collect();
        let mut uniq = codes.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), codes.len(), "no duplicates");
        assert_eq!(codes.len(), 50);
        for kc in &codes {
            assert!(!layout::US_MODIFIERS.contains(kc));
            assert_ne!(*kc, layout::ESCAPE);
            // Each one is a character key in the US layout, or IntlBackslash /
            // IntlRo / IntlYen.
            let us_char = layout::US
                .iter()
                .any(|&(c, l1, _)| c == *kc && (0x20..0x7f).contains(&l1));
            assert!(us_char || [94, 97, 132].contains(kc), "code {kc}");
        }
        // Named keys a client acts on are not among them.
        for kc in [22, 23, 36, 65, 103, 119] {
            assert!(!codes.contains(&kc), "code {kc}");
        }
    }

    #[test]
    fn type_pages_split_where_the_keymap_is_full() {
        // CJK has no US position: each takes one printable code.
        let cjk = |n: u32| -> Vec<u32> { (0..n).map(|i| 0x0100_0000 | (0x4e00 + i)).collect() };
        let pages = type_pages(&cjk(50));
        assert_eq!(pages.len(), 1);
        let pages = type_pages(&cjk(51));
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].slots.len(), 50);
        assert_eq!(pages[1].slots, vec![0]);

        // Repeats reuse the slot and never open a page.
        let mut syms = cjk(50);
        syms.extend(cjk(50));
        let pages = type_pages(&syms);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].slots[50..], (0..50).collect::<Vec<_>>()[..]);

        // Each page types its own characters, in order, and nothing is lost.
        let text = "The quick brown fox jumps over the lazy dog. THE QUICK BROWN FOX! 0123456789 (§$%&/=?) äöüß ÄÖÜ €@ {[]}\\ <>|~#'\"";
        let keysyms: Vec<u32> = text.chars().map(|c| keysym_for_char(c).unwrap()).collect();
        let pages = type_pages(&keysyms);
        assert!(pages.len() >= 2, "{} pages", pages.len());
        let mut typed = Vec::new();
        for page in &pages {
            // A page's keymap holds each of its keys exactly once.
            let mut local: Vec<u32> = Vec::new();
            let mut it = keysyms[typed.len()..].iter();
            for &slot in &page.slots {
                let ks = *it.next().unwrap();
                if slot == local.len() {
                    local.push(ks);
                }
                assert_eq!(local[slot], ks);
                typed.push(ks);
            }
            assert_eq!(page.keymap.placements.len(), local.len());
        }
        assert_eq!(typed, keysyms);
    }

    #[test]
    fn combo_spill_prefers_codes_chromium_knows() {
        // IntlRo and IntlYen first: a spilled combo key stays visible to
        // Chromium for the first two keys.
        assert_eq!(&layout::US_UNMAPPED[..2], &[97, 132]);
        let km = generate_keymap(&[0xe4, 0xf6]); // ä ö
        assert_eq!(km.placements[0].evdev_code, 97 - 8);
        assert_eq!(km.placements[1].evdev_code, 132 - 8);
    }

    #[test]
    fn modifier_evdev_code_maps_tokens() {
        let km = generate_keymap(&[keysym_for_char('x').unwrap()]);
        assert_eq!(modifier_evdev_code(&km, "ctrl").unwrap(), km.ctrl_code);
        assert_eq!(modifier_evdev_code(&km, "control").unwrap(), km.ctrl_code);
        assert_eq!(modifier_evdev_code(&km, "shift").unwrap(), km.shift_code);
        assert_eq!(modifier_evdev_code(&km, "alt").unwrap(), km.alt_code);
        assert_eq!(modifier_evdev_code(&km, "cmd").unwrap(), km.super_code);
        assert_eq!(modifier_evdev_code(&km, "super").unwrap(), km.super_code);
        assert!(modifier_evdev_code(&km, "a").is_err());
    }
}
