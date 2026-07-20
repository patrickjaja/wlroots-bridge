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

    bail!("unsupported key name `{name}` (resolved keysym name `{resolved}`)")
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

/// XKB reserves keycodes 0..=8; real keys start at 9. We lay out ours from a
/// fixed base so the first placement lands at a predictable code.
const XKB_KEYCODE_BASE: u32 = 9;

/// Build an XKB keymap text that binds the four modifier keys plus every keysym
/// in `keysyms` to sequential keycodes.
///
/// Layout (XKB keycode -> role):
///   base+0 = Control_L, base+1 = Shift_L, base+2 = Alt_L, base+3 = Super_L,
///   base+4.. = the requested keysyms (one per keycode; level 1 = the keysym,
///   level 2 = the keysym again so Shift never changes it - we drive Shift as a
///   real modifier when a keysym needs it).
///
/// The `key` request wants the evdev code (= XKB keycode - 8), so the returned
/// placements carry `xkb_keycode - 8`.
pub fn generate_keymap(keysyms: &[u32]) -> GeneratedKeymap {
    // Modifier keycodes (XKB space).
    let ctrl_kc = XKB_KEYCODE_BASE;
    let shift_kc = XKB_KEYCODE_BASE + 1;
    let alt_kc = XKB_KEYCODE_BASE + 2;
    let super_kc = XKB_KEYCODE_BASE + 3;
    let first_key_kc = XKB_KEYCODE_BASE + 4;

    let max_kc = first_key_kc + keysyms.len() as u32; // one past the last used

    // --- xkb_keycodes: declare <Kxx> aliases for every keycode we use. ---
    let mut keycodes = String::new();
    keycodes.push_str("xkb_keycodes \"wlroots-bridge\" {\n");
    keycodes.push_str(&format!("  minimum = {XKB_KEYCODE_BASE};\n"));
    keycodes.push_str(&format!("  maximum = {};\n", max_kc.max(first_key_kc)));
    for kc in XKB_KEYCODE_BASE..max_kc.max(first_key_kc) {
        keycodes.push_str(&format!("  <K{kc}> = {kc};\n"));
    }
    keycodes.push_str("};\n");

    // --- xkb_types: use the stock "complete" types (compositor ships them). ---
    let types = "xkb_types \"wlroots-bridge\" { include \"complete\" };\n";

    // --- xkb_compat: stock rules so modifiers behave. ---
    let compat = "xkb_compat \"wlroots-bridge\" { include \"complete\" };\n";

    // --- xkb_symbols: bind modifiers + each keysym, and the modifier_map. ---
    let mut symbols = String::new();
    symbols.push_str("xkb_symbols \"wlroots-bridge\" {\n");
    symbols.push_str(&format!("  key <K{ctrl_kc}> {{ [ Control_L ] }};\n"));
    symbols.push_str(&format!("  key <K{shift_kc}> {{ [ Shift_L ] }};\n"));
    symbols.push_str(&format!("  key <K{alt_kc}> {{ [ Alt_L ] }};\n"));
    symbols.push_str(&format!("  key <K{super_kc}> {{ [ Super_L ] }};\n"));

    let mut placements = Vec::with_capacity(keysyms.len());
    for (i, &ks) in keysyms.iter().enumerate() {
        let kc = first_key_kc + i as u32;
        let sym = xkb_symbol_name(ks);
        // Two identical levels: the same symbol whether or not Shift is held, so
        // a stray Shift never corrupts the glyph. Shift is only driven for
        // keysyms whose canonical placement is a shifted glyph (handled below by
        // needs_shift heuristic), but here we bind the exact keysym at level 1.
        symbols.push_str(&format!("  key <K{kc}> {{ [ {sym}, {sym} ] }};\n"));
        placements.push(KeyPlacement {
            evdev_code: kc - 8,
            // We bind the exact keysym at level 1, so Shift is never needed to
            // produce it. (For an uppercase 'A' we bind Aacute-style directly.)
            needs_shift: false,
        });
    }

    symbols.push_str(&format!("  modifier_map Control {{ <K{ctrl_kc}> }};\n"));
    symbols.push_str(&format!("  modifier_map Shift {{ <K{shift_kc}> }};\n"));
    symbols.push_str(&format!("  modifier_map Mod1 {{ <K{alt_kc}> }};\n"));
    symbols.push_str(&format!("  modifier_map Mod4 {{ <K{super_kc}> }};\n"));
    symbols.push_str("};\n");

    let text = format!("xkb_keymap {{\n{keycodes}{types}{compat}{symbols}}};\n");

    GeneratedKeymap {
        text,
        ctrl_code: ctrl_kc - 8,
        shift_code: shift_kc - 8,
        alt_code: alt_kc - 8,
        super_code: super_kc - 8,
        placements,
    }
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

/// The XKB real-modifier mask bit for a modifier token, matching the
/// `modifier_map` emitted by [`generate_keymap`] (Control=0x4, Shift=0x1,
/// Mod1/Alt=0x8, Mod4/Super=0x40).
///
/// The virtual keyboard must announce this via
/// `zwp_virtual_keyboard_v1.modifiers`: Smithay-based compositors (e.g. niri)
/// do not derive modifier state from injected modifier *keycodes*, only from the
/// explicit `modifiers` request. wlroots derives it from the `modifier_map`,
/// which is why key-only chording worked on Sway/Hyprland but dropped the
/// modifier on niri.
pub fn modifier_mask(token: &str) -> Result<u32> {
    match modifier_keysym_name(token) {
        Some("Control_L") => Ok(0x0000_0004),
        Some("Shift_L") => Ok(0x0000_0001),
        Some("Alt_L") => Ok(0x0000_0008),
        Some("Super_L") => Ok(0x0000_0040),
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

        // One placement per requested keysym, codes strictly increasing.
        assert_eq!(km.placements.len(), 3);
        assert!(km.placements[0].evdev_code < km.placements[1].evdev_code);
        assert!(km.placements[1].evdev_code < km.placements[2].evdev_code);

        // Modifier evdev codes distinct from key codes.
        assert!(km.ctrl_code < km.placements[0].evdev_code);
        assert!(km.shift_code < km.placements[0].evdev_code);

        // The CJK char is placed as a Unicode symbol.
        assert!(km.text.contains("U597D")); // 好 = U+597D
        // modifier_map entries present.
        assert!(km.text.contains("modifier_map Control"));
        assert!(km.text.contains("modifier_map Shift"));
    }

    #[test]
    fn generated_keymap_handles_repeated_chars() {
        // Repeated chars each get their own keycode (we don't dedupe); the
        // keymap must still be well-formed and one placement per input.
        let syms: Vec<u32> = "aaa".chars().map(|c| keysym_for_char(c).unwrap()).collect();
        let km = generate_keymap(&syms);
        assert_eq!(km.placements.len(), 3);
        let opens = km.text.matches('{').count();
        let closes = km.text.matches('}').count();
        assert_eq!(opens, closes);
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
