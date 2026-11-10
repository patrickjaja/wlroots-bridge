//! Hyprland backend of `abort-watch`.
//!
//! Escape on a physical keyboard runs Hyprland's `event` dispatcher, which
//! writes `custom>>claude-cu-abort` to socket2; every armed process reads its
//! own socket2 connection. The bind is NON-CONSUMING (the focused app still
//! gets the Escape) and bypasses shortcut inhibitors (like sway's
//! `--inhibited`).
//!
//! Dialect, from the read-only `j/status` (`configProvider`, 0.55+; an
//! `eval` probe would clear the user's config error list):
//!
//! - Lua config (`lua`, 0.55+, the only one from 0.57): `eval` runs
//!   `hl.bind("EsCaPe", hl.dsp.event(..), {non_consuming, dont_inhibit,
//!   description, device = {inclusive, list}})`; the bind objects are kept in
//!   the Lua global [`cmd::LUA_TABLE`] and removed with `:remove()`.
//! - hyprlang (`hyprlang` on 0.55/0.56, `unknown request` on 0.54 and older):
//!   `keyword bindnpkd = , EsCaPe, <devices>, ..` (the `k` per-device flag is
//!   0.55+), else a global `keyword bindnpd` (no device scoping:
//!   `syntheticSafe: false`; `suspend` unbinds it, counted across holders).
//!
//! The key is spelled `EsCaPe` in both dialects: hyprlang `unbind` and the
//! 0.55/0.56 Lua `remove()` erase every bind whose key STRING (and mods)
//! match, so the odd spelling keeps the user's own Escape binds (submap exits)
//! out of reach; the keysym still resolves case-insensitively to Escape.
//! (`hl.unbind("Escape")` is never used: it normalizes case.)
//!
//! Physical keyboards come from `j/devices`: the bridge's virtual keyboard is
//! `hl-virtual-keyboard[-<proc>][-N]` and is never listed. Hyprland sends no
//! device hot-plug event, so the device list is re-read every [`DEVICE_POLL`]
//! (also under constant socket2 traffic) and the bind redone when it changed.
//! A `configreloaded` event re-binds (after re-checking the user's binds)
//! when ours is gone, unless the global bind is suspended.
//!
//! The user's own bare Escape bind outside submaps (from `j/binds`, plus the
//! 0.57 text `binds` for `ignore_mods`) makes arming fail.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cu_abort_watch::{AbortSink, Backend, BoxError, Mechanism, Ready, SharedBinding};
use serde_json::Value;

use super::{Debounce, MARKER, Probe, Record, log};
use crate::ax::IPC_TIMEOUT;

/// How often the device list is re-read (Hyprland has no hot-plug event).
pub const DEVICE_POLL: Duration = Duration::from_secs(2);
const THREAD_JOIN: Duration = Duration::from_secs(2);

/// The request socket, the event socket and the signature of one Hyprland
/// instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub sock: PathBuf,
    pub sock2: PathBuf,
    pub signature: String,
}

impl Paths {
    pub fn from_env() -> Option<Self> {
        let sock = crate::ax::hypr_socket()?;
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        Some(Self {
            sock2: sock.with_file_name(".socket2.sock"),
            sock,
            signature,
        })
    }
}

/// Every Hyprland string this backend depends on, in one place. Each one is
/// version-dependent; see the VM checklist in DESIGN.md.
pub mod cmd {
    use super::MARKER;

    /// Read-only dialect probe: `{"configProvider": "lua"|"hyprlang", ..}` on
    /// 0.55+, `unknown request` on 0.54 and older (hyprlang).
    pub const STATUS: &str = "j/status";
    /// Lua global holding our bind objects.
    pub const LUA_TABLE: &str = "__claude_cu_abort";
    /// Key spelling unique to our bind (see module docs).
    pub const KEY: &str = "EsCaPe";
    /// socket2 line our dispatcher produces.
    pub fn abort_event() -> String {
        format!("custom>>{MARKER}")
    }
    /// socket2 line after a config reload.
    pub const RELOAD_PREFIX: &str = "configreloaded>>";
    /// Request replies that mean success.
    pub fn is_ok(reply: &str) -> bool {
        reply.trim() == "ok"
    }
    /// hyprlang `bind` reply for an unknown flag.
    pub fn is_invalid_flag(reply: &str) -> bool {
        reply.contains("invalid flag")
    }

    /// A Lua string literal (`"..."`, every non-printable or non-ASCII byte
    /// as `\ddd`).
    pub fn lua_str(s: &str) -> String {
        let mut out = String::from("\"");
        for b in s.bytes() {
            match b {
                b'"' => out.push_str("\\\""),
                b'\\' => out.push_str("\\\\"),
                0x20..=0x7e => out.push(b as char),
                _ => out.push_str(&format!("\\{b:03}")),
            }
        }
        out.push('"');
        out
    }

    /// Lua (0.55+): one bind for all `devices`, kept in [`LUA_TABLE`].
    /// Option keys from `hlBind` at v0.55.0, v0.56.0 and v0.57.2.
    pub fn lua_bind(devices: &[String]) -> String {
        let list = devices
            .iter()
            .map(|d| lua_str(d))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "eval local t = {LUA_TABLE} or {{}}\n{LUA_TABLE} = t\nt[#t + 1] = hl.bind({k}, hl.dsp.event({m}), {{ non_consuming = true, dont_inhibit = true, description = {m}, device = {{ inclusive = true, list = {{ {list} }} }} }})",
            k = lua_str(KEY),
            m = lua_str(MARKER)
        )
    }

    /// `tostring()` of a bind object whose bind is gone (`keybindToString`,
    /// null-safe in 0.55, 0.56 and 0.57).
    pub const EXPIRED: &str = "HL.Keybind(expired)";

    /// Lua: remove exactly the binds in [`LUA_TABLE`], skipping expired
    /// objects. On 0.55/0.56 `remove()` on an expired object dereferences a
    /// null pointer (`getKeybindFromUserdata` returns an always-set
    /// optional) and crashes the compositor; `pcall` cannot catch a SIGSEGV.
    /// A bind expires when something else removed it (a user's
    /// `hl.unbind("escape")`, a config tool), so only `tostring` may touch a
    /// stored object before `remove()`.
    pub fn lua_unbind() -> String {
        format!(
            "eval for _, b in ipairs({LUA_TABLE} or {{}}) do if tostring(b) ~= {e} then pcall(function() b:remove() end) end end\n{LUA_TABLE} = nil",
            e = lua_str(EXPIRED)
        )
    }

    /// hyprlang 0.55/0.56: `bind` + `n` non-consuming, `p` bypass inhibitors,
    /// `k` per-device, `d` description (flags are single characters in any
    /// order): `bindnpkd = MODS, KEY, DEVICES, DESCRIPTION, DISPATCHER, ARG`
    /// (devices space-separated; a leading `!` would invert the list).
    pub fn legacy_bind_devices(devices: &[String]) -> String {
        format!(
            "keyword bindnpkd , {KEY}, {}, {MARKER}, event, {MARKER}",
            devices.join(" ")
        )
    }

    /// hyprlang without per-device binds (0.54 and older): every keyboard,
    /// virtual ones too. Tried in order: `p` first, then without it for
    /// releases that predate `p`.
    pub fn legacy_bind_global() -> [String; 2] {
        [
            format!("keyword bindnpd , {KEY}, {MARKER}, event, {MARKER}"),
            format!("keyword bindnd , {KEY}, {MARKER}, event, {MARKER}"),
        ]
    }

    /// hyprlang: removes every bind with no mods whose key string is exactly
    /// [`KEY`] (ours only).
    pub fn legacy_unbind() -> String {
        format!("keyword unbind , {KEY}")
    }

    /// Keyboards that inject synthetic keys: Hyprland names every
    /// `zwp_virtual_keyboard_v1` `hl-virtual-keyboard` (`-<process>` with
    /// `misc:name_vk_after_proc`, `-N` for duplicates); ydotool's uinput
    /// device. Remappers (keyd, kanata) carry the user's keys and stay.
    pub fn is_injector_name(name: &str) -> bool {
        name.starts_with("hl-virtual-keyboard")
            || name.starts_with("virtual-keyboard")
            || name.contains("ydotool")
    }

    /// A device name the hyprlang device list can carry: Hyprland already
    /// turns spaces and commas in names into `-`; a leading `!` would invert
    /// the list.
    pub fn legacy_safe_name(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('!')
            && !name.contains(',')
            && !name.chars().any(char::is_whitespace)
    }
}

/// How our bind is made; decides `syntheticSafe` and the unbind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Lua,
    LegacyDevices,
    LegacyGlobal,
}

impl Variant {
    pub fn encode(self) -> &'static str {
        match self {
            Variant::Lua => "hypr:lua",
            Variant::LegacyDevices => "hypr:legacy-devices",
            Variant::LegacyGlobal => "hypr:legacy-global",
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        match s {
            "hypr:lua" => Some(Variant::Lua),
            "hypr:legacy-devices" => Some(Variant::LegacyDevices),
            "hypr:legacy-global" => Some(Variant::LegacyGlobal),
            _ => None,
        }
    }

    pub fn synthetic_safe(self) -> bool {
        self != Variant::LegacyGlobal
    }
}

// ----------------------------------------------------------- requests

/// One request on a fresh connection; the reply as text (1.5 s timeouts).
pub fn raw(path: &Path, req: &str) -> Result<String> {
    let mut s = UnixStream::connect(path).context("connect Hyprland socket")?;
    s.set_read_timeout(Some(IPC_TIMEOUT))?;
    s.set_write_timeout(Some(IPC_TIMEOUT))?;
    s.write_all(req.as_bytes())?;
    let mut body = Vec::new();
    s.take(64 << 20).read_to_end(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn json(path: &Path, req: &str) -> Result<Value> {
    let t = raw(path, &format!("j/{req}"))?;
    serde_json::from_str(&t).with_context(|| format!("Hyprland `{req}` reply is not JSON"))
}

/// The config dialect, from the `j/status` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `configProvider: lua` (0.55+).
    Lua,
    /// `configProvider: hyprlang` (0.55/0.56).
    Hyprlang,
    /// `unknown request`: 0.54 or older, hyprlang.
    OldHyprlang,
}

/// Strict: any other reply is an error (unverified means not armed).
pub fn parse_status(reply: &str) -> Result<Dialect> {
    if reply.trim() == "unknown request" {
        return Ok(Dialect::OldHyprlang);
    }
    let v: Value = serde_json::from_str(reply)
        .with_context(|| format!("unexpected Hyprland status reply: {:.80}", reply.trim()))?;
    match v.get("configProvider").and_then(Value::as_str) {
        Some("lua") => Ok(Dialect::Lua),
        Some("hyprlang") => Ok(Dialect::Hyprlang),
        other => bail!("unknown Hyprland configProvider {other:?}"),
    }
}

fn dialect(sock: &Path) -> Result<Dialect> {
    parse_status(&raw(sock, cmd::STATUS)?)
}

/// Physical keyboard names from a `j/devices` reply (sorted, unique).
pub fn physical_names(devices: &Value) -> Vec<String> {
    let mut out: Vec<String> = devices
        .get("keyboards")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k.get("name").and_then(Value::as_str))
        .filter(|n| !n.is_empty() && !cmd::is_injector_name(n))
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true" || s == "1",
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

fn is_ours(b: &Value) -> bool {
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("");
    s("description") == MARKER
        || (s("dispatcher").eq_ignore_ascii_case("event") && s("arg") == MARKER)
}

/// Whether one of our binds is registered (from a `j/binds` reply).
pub fn ours_present(binds: &Value) -> bool {
    binds.as_array().is_some_and(|l| l.iter().any(is_ours))
}

fn is_escape_key(key: &str, keycode: Option<u64>) -> bool {
    key.eq_ignore_ascii_case("escape") || keycode == Some(9)
}

/// The user's bare Escape binds active outside submaps (from `j/binds`).
pub fn user_escape_binds(binds: &Value) -> Vec<String> {
    let Some(list) = binds.as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter(|b| !is_ours(b))
        .filter(|b| b.get("modmask").and_then(Value::as_u64) == Some(0))
        .filter(|b| !truthy(b.get("catch_all")) && !truthy(b.get("mouse")))
        .filter(|b| {
            b.get("submap")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
                || truthy(b.get("submap_universal"))
        })
        .filter(|b| {
            is_escape_key(
                b.get("key").and_then(Value::as_str).unwrap_or(""),
                b.get("keycode").and_then(Value::as_u64),
            )
        })
        .map(|b| describe(b, "Escape"))
        .collect()
}

/// How a user bind is named in messages: its description when it has one
/// (Lua binds report the dispatcher as `__lua <ref>`), else dispatcher + arg.
fn describe(b: &Value, key: &str) -> String {
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").trim();
    if !s("description").is_empty() {
        format!("{key} -> \"{}\"", s("description"))
    } else {
        format!("{key} -> {} {}", s("dispatcher"), s("arg"))
            .trim()
            .to_owned()
    }
}

/// Escape binds with `ignore_mods` (they fire with any modifiers, so they
/// conflict like a bare one) from the plain-text `binds` reply. Only 0.57+
/// prints a `flags:` line; older text has none and yields nothing.
pub fn ignore_mods_escape_binds(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut f: HashMap<&str, &str> = HashMap::new();
        for line in block.lines() {
            if let Some((k, v)) = line.trim().split_once(':') {
                f.insert(k.trim(), v.trim());
            }
        }
        let get = |k: &str| f.get(k).copied().unwrap_or("");
        let ignore = get("flags").split(',').any(|x| x.trim() == "ignore_mods");
        let escape = is_escape_key(get("key"), get("keycode").parse().ok());
        let global = get("submap").is_empty();
        let ours = get("description") == MARKER;
        if ignore && escape && global && !ours {
            let v = serde_json::json!({
                "description": get("description"),
                "dispatcher": get("dispatcher"),
                "arg": get("arg"),
            });
            out.push(describe(&v, "Escape (ignore_mods)"));
        }
    }
    out
}

// --------------------------------------------------------------- core

#[derive(Clone)]
struct Core {
    paths: Paths,
    set: SharedBinding,
    /// Holders currently suspending the global bind (`<instance>-suspended`).
    susp: SharedBinding,
    lua: bool,
}

impl Core {
    fn dir(&self) -> &Path {
        self.set.dir()
    }

    fn physical(&self) -> Result<Vec<String>> {
        Ok(physical_names(&json(&self.paths.sock, "devices")?))
    }

    fn binds(&self) -> Result<Value> {
        let v = json(&self.paths.sock, "binds")?;
        if !v.is_array() {
            bail!("unexpected Hyprland binds reply");
        }
        Ok(v)
    }

    /// The user's conflicting Escape bind, if any.
    fn conflict(&self, binds: &Value) -> Option<String> {
        if let Some(c) = user_escape_binds(binds).into_iter().next() {
            return Some(c);
        }
        let text = raw(&self.paths.sock, "binds").ok()?;
        ignore_mods_escape_binds(&text).into_iter().next()
    }

    fn send(&self, req: &str) -> Result<()> {
        let reply = raw(&self.paths.sock, req)?;
        if cmd::is_ok(&reply) {
            Ok(())
        } else {
            Err(anyhow!("{}", reply.trim()))
        }
    }

    fn bind_global(&self) -> Result<()> {
        let [with_p, without_p] = cmd::legacy_bind_global();
        match self.send(&with_p) {
            Err(e) if cmd::is_invalid_flag(&e.to_string()) => self
                .send(&without_p)
                .context("Hyprland refused the Escape bind"),
            r => r.context("Hyprland refused the Escape bind"),
        }
    }

    /// Bind for `devices`. `known` = the spelling in use; `None` on hyprlang
    /// = learn it (per-device, else global).
    fn bind(&self, devices: &[String], known: Option<Variant>) -> Result<Variant> {
        if self.lua {
            if !devices.is_empty() {
                self.send(&cmd::lua_bind(devices))
                    .context("Hyprland refused the Lua Escape bind")?;
            }
            return Ok(Variant::Lua);
        }
        if known == Some(Variant::LegacyGlobal) {
            self.bind_global()?;
            return Ok(Variant::LegacyGlobal);
        }
        let names: Vec<String> = devices
            .iter()
            .filter(|d| {
                let ok = cmd::legacy_safe_name(d);
                if !ok {
                    log(format!("skipping keyboard with unusable name {d:?}"));
                }
                ok
            })
            .cloned()
            .collect();
        if names.is_empty() {
            return Ok(Variant::LegacyDevices);
        }
        match self.send(&cmd::legacy_bind_devices(&names)) {
            Ok(()) => Ok(Variant::LegacyDevices),
            Err(e) if known.is_none() && cmd::is_invalid_flag(&e.to_string()) => {
                log("no per-device binds (Hyprland < 0.55): global bind, syntheticSafe false");
                self.bind_global()?;
                Ok(Variant::LegacyGlobal)
            }
            Err(e) => Err(e.context("Hyprland refused the Escape bind")),
        }
    }

    fn unbind(&self) {
        let req = if self.lua {
            cmd::lua_unbind()
        } else {
            cmd::legacy_unbind()
        };
        if let Err(e) = self.send(&req) {
            log(format!("unbind failed: {e:#}"));
        }
    }

    fn record_variant(&self) -> Option<Variant> {
        Record::read(self.dir()).and_then(|r| Variant::decode(&r.variant))
    }

    fn clear_stale(&self) {
        log("clearing binds left by a crashed holder");
        self.unbind();
        Record::remove(self.dir());
        if let Ok(b) = self.binds()
            && ours_present(&b)
        {
            log("binds from a crashed holder are still registered; their presses are merged");
        }
    }

    /// Whether some live holder suspends the global bind right now.
    fn suspended_by_anyone(&self) -> bool {
        match self.susp.lock() {
            Ok(l) => l.others() > 0 || l.is_registered(),
            Err(_) => false,
        }
    }

    /// Join the holder set (first holder: clear stale, check, bind).
    fn join(&self) -> Result<Variant, BoxError> {
        let mut lock = self.set.lock()?;
        if !lock.is_first() {
            lock.register()?;
            return Ok(self.record_variant().unwrap_or(if self.lua {
                Variant::Lua
            } else {
                Variant::LegacyDevices
            }));
        }
        if lock.pruned() > 0 || Record::exists(self.dir()) {
            self.clear_stale();
        }
        if let Some(c) = self.conflict(&self.binds()?) {
            return Err(format!(
                "Escape is already bound in Hyprland ({c}); not overriding the user's binding"
            )
            .into());
        }
        let devices = self.physical()?;
        if devices.is_empty() {
            log("no physical keyboard yet: binding when one appears");
        }
        Record {
            variant: String::new(),
            devices: devices.clone(),
        }
        .write(self.dir())?;
        lock.register()?;
        let bound = self.bind(&devices, None).and_then(|v| {
            // Unverified means not armed: the bind must show up in `j/binds`.
            let ran = self.lua || v == Variant::LegacyGlobal || !devices.is_empty();
            if ran && !ours_present(&self.binds()?) {
                bail!("Hyprland answered ok but the Escape bind is not in `binds`");
            }
            Ok(v)
        });
        match bound {
            Ok(v) => {
                // A hyprlang bind that did not run has not proven `k`.
                let proven = self.lua || v == Variant::LegacyGlobal || !devices.is_empty();
                Record {
                    variant: if proven {
                        v.encode().into()
                    } else {
                        String::new()
                    },
                    devices,
                }
                .write(self.dir())?;
                Ok(v)
            }
            Err(e) => {
                self.unbind();
                Record::remove(self.dir());
                let _ = lock.unregister();
                Err(format!("{e:#}").into())
            }
        }
    }

    /// Poll tick: redo the per-device bind when the keyboards changed.
    fn sync_devices(&self) -> Result<()> {
        let lock = self.set.lock()?;
        if !lock.is_registered() {
            return Ok(());
        }
        let mut rec = Record::read(self.dir()).unwrap_or_default();
        let known = Variant::decode(&rec.variant);
        if known == Some(Variant::LegacyGlobal) {
            if !self.suspended_by_anyone() && !ours_present(&self.binds()?) {
                log("our Escape bind was removed at run time: binding again");
                self.bind_global()?;
            }
            return Ok(());
        }
        let now = self.physical()?;
        // Something else may have removed our bind at run time (a user's
        // `hl.unbind("escape")`, `keyword unbind`, a config tool): there is
        // no event for it, so check every poll.
        let expected = !rec.devices.is_empty() && known.is_some();
        let missing = expected && !ours_present(&self.binds()?);
        if now == rec.devices && !missing {
            return Ok(());
        }
        if missing {
            log("our Escape bind was removed at run time: binding again");
        } else {
            log(format!("keyboards changed: {:?} -> {now:?}", rec.devices));
        }
        // The guarded unbind also empties the Lua table of expired objects.
        self.unbind();
        let v = self.bind(&now, known)?;
        rec.variant = v.encode().into();
        rec.devices = now;
        rec.write(self.dir())?;
        Ok(())
    }

    /// After `configreloaded`: re-bind when our bind is gone.
    fn after_reload(&self) -> Result<()> {
        let lock = self.set.lock()?;
        if !lock.is_registered() {
            return Ok(());
        }
        let binds = self.binds()?;
        if let Some(c) = self.conflict(&binds) {
            bail!("Hyprland reload added an Escape bind ({c}); Escape abort stopped");
        }
        if ours_present(&binds) {
            return Ok(());
        }
        let mut rec = Record::read(self.dir()).unwrap_or_default();
        let known = Variant::decode(&rec.variant);
        if known == Some(Variant::LegacyGlobal) && self.suspended_by_anyone() {
            return Ok(()); // the last `resume` binds again
        }
        let now = self.physical()?;
        let v = self.bind(&now, known)?;
        rec.variant = v.encode().into();
        rec.devices = now;
        rec.write(self.dir())?;
        log("re-bound after Hyprland config reload");
        Ok(())
    }

    fn unbind_all(&self) {
        self.unbind();
        Record::remove(self.dir());
    }
}

/// `doctor`: the socket answers and no user Escape bind is in the way.
/// Read-only requests only.
pub fn probe(paths: &Paths) -> std::result::Result<Probe, String> {
    let d = dialect(&paths.sock).map_err(|e| format!("Hyprland IPC: {e:#}"))?;
    let lua = d == Dialect::Lua;
    let binds = json(&paths.sock, "binds").map_err(|e| format!("Hyprland IPC: {e:#}"))?;
    let mut conflicts = user_escape_binds(&binds);
    if let Ok(text) = raw(&paths.sock, "binds") {
        conflicts.extend(ignore_mods_escape_binds(&text));
    }
    if let Some(c) = conflicts.into_iter().next() {
        return Err(format!("Escape is already bound in Hyprland ({c})"));
    }
    let skipped = if lua {
        Vec::new()
    } else {
        json(&paths.sock, "devices")
            .map(|d| physical_names(&d))
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !cmd::legacy_safe_name(n))
            .collect()
    };
    Ok(Probe {
        skipped,
        // The bind is non-consuming (`non_consuming` / `n`, both present in
        // 0.55+ source). Before 0.55 the version is not reported here.
        consumes: match d {
            Dialect::Lua | Dialect::Hyprlang => Some(false),
            Dialect::OldHyprlang => None,
        },
    })
}

// ------------------------------------------------------------ backend

pub struct HyprBackend {
    paths: Paths,
    shared_dir: Option<PathBuf>,
    armed: Option<Armed>,
}

struct Armed {
    core: Core,
    variant: Variant,
    stop: Arc<AtomicBool>,
    sub: UnixStream,
    done: mpsc::Receiver<()>,
}

impl HyprBackend {
    pub fn new(paths: Paths, shared_dir: Option<PathBuf>) -> Self {
        Self {
            paths,
            shared_dir,
            armed: None,
        }
    }
}

/// Split socket2 bytes into complete lines; the rest stays in `pending`.
pub fn take_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(i) = pending.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = pending.drain(..=i).collect();
        out.push(
            String::from_utf8_lossy(&line[..i])
                .trim_end_matches('\r')
                .to_owned(),
        );
    }
    out
}

fn event_loop(mut sub: UnixStream, core: Core, sink: AbortSink, stop: Arc<AtomicBool>) {
    let debounce = Debounce::default();
    let abort = cmd::abort_event();
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    let mut last_sync = Instant::now();
    loop {
        match sub.read(&mut buf) {
            Ok(0) => {
                if !stop.load(Ordering::SeqCst) {
                    sink.fail("Hyprland socket2 closed");
                }
                return;
            }
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                if pending.len() > 1 << 20 {
                    pending.clear(); // a runaway line without newline
                }
                for line in take_lines(&mut pending) {
                    if line == abort {
                        debounce.abort(&sink);
                    } else if line.starts_with(cmd::RELOAD_PREFIX)
                        && let Err(e) = core.after_reload()
                    {
                        sink.fail(format!("{e:#}"));
                        return;
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                if !stop.load(Ordering::SeqCst) {
                    sink.fail(format!("Hyprland socket2: {e}"));
                }
                return;
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Poll by elapsed time, so steady socket2 traffic cannot starve it.
        if last_sync.elapsed() >= DEVICE_POLL {
            last_sync = Instant::now();
            if let Err(e) = core.sync_devices() {
                log(format!("device sync: {e:#}"));
            }
        }
    }
}

impl Backend for HyprBackend {
    fn arm(&mut self, sink: AbortSink) -> std::result::Result<Ready, BoxError> {
        let set = match &self.shared_dir {
            Some(d) => SharedBinding::in_dir(d)?,
            None => super::instance_set(
                Mechanism::Hyprland,
                &super::hypr_instance_key(&self.paths.signature),
            )?,
        };
        let name = set
            .dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let susp = SharedBinding::in_dir(set.dir().with_file_name(format!("{name}-suspended")))?;
        let core = Core {
            lua: dialect(&self.paths.sock)? == Dialect::Lua,
            paths: self.paths.clone(),
            set,
            susp,
        };
        // Listen before binding so the first press is never missed. Every
        // fallible step without a side effect comes before `join`.
        let sub = UnixStream::connect(&self.paths.sock2).context("connect Hyprland socket2")?;
        sub.set_read_timeout(Some(DEVICE_POLL))?;
        let thread_sub = sub.try_clone()?;
        let variant = match core.join() {
            Ok(v) => v,
            Err(e) => {
                let _ = sub.shutdown(std::net::Shutdown::Both);
                return Err(e);
            }
        };
        // From here on an error must undo the join (holder file, record,
        // bind): `arm` leaves nothing behind on Err.
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, done) = mpsc::channel();
        let (c, s) = (core.clone(), stop.clone());
        let spawned = super::injected_failure_after_bind().and_then(|()| {
            std::thread::Builder::new()
                .name("hypr-events".into())
                .spawn(move || {
                    event_loop(thread_sub, c, sink, s);
                    let _ = tx.send(());
                })
                .map(drop)
                .map_err(BoxError::from)
        });
        if let Err(e) = spawned {
            stop.store(true, Ordering::SeqCst);
            let _ = sub.shutdown(std::net::Shutdown::Both);
            let undo = core.clone();
            if let Err(r) = core.set.release(|| undo.unbind_all()) {
                log(format!("release after a failed arm: {r}"));
            }
            return Err(e);
        }
        self.armed = Some(Armed {
            core,
            variant,
            stop,
            sub,
            done,
        });
        Ok(Ready {
            mechanism: Mechanism::Hyprland,
            synthetic_safe: variant.synthetic_safe(),
        })
    }

    /// Only the global (not device-scoped) bind needs this: it is removed so
    /// the model's Escape reaches the app without aborting. Counted across
    /// holders: the first suspender unbinds, the last resumer binds again.
    fn suspend(&mut self) -> std::result::Result<(), BoxError> {
        let Some(a) = &self.armed else { return Ok(()) };
        if a.variant != Variant::LegacyGlobal {
            return Ok(());
        }
        let core = a.core.clone();
        a.core.susp.acquire(move || {
            core.send(&cmd::legacy_unbind())?;
            Ok(())
        })?;
        Ok(())
    }

    fn resume(&mut self) -> std::result::Result<(), BoxError> {
        let Some(a) = &self.armed else { return Ok(()) };
        if a.variant != Variant::LegacyGlobal {
            return Ok(());
        }
        let failed: RefCell<Option<String>> = RefCell::new(None);
        let core = a.core.clone();
        let r = a.core.susp.release(|| {
            let r = core.binds().and_then(|b| {
                if ours_present(&b) {
                    Ok(())
                } else {
                    core.bind_global()
                }
            });
            if let Err(e) = r {
                *failed.borrow_mut() = Some(format!("{e:#}"));
            }
        });
        if let Err(e) = r {
            log(format!("resume release: {e}"));
        }
        match failed.into_inner() {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    fn disarm(&mut self) {
        let Some(a) = self.armed.take() else { return };
        a.stop.store(true, Ordering::SeqCst);
        let _ = a.sub.shutdown(std::net::Shutdown::Both);
        let _ = a.done.recv_timeout(THREAD_JOIN);
        // Leaving while suspended: give the global bind back to the others.
        if a.variant == Variant::LegacyGlobal && a.core.susp.lock().is_ok_and(|l| l.is_registered())
        {
            let core = a.core.clone();
            let _ = a.core.susp.release(|| {
                let _ = core.bind_global();
            });
        }
        let core = a.core.clone();
        if let Err(e) = a.core.set.release(|| core.unbind_all()) {
            log(format!("release: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_strings() {
        let devs = vec![
            "at-translated-set-2-keyboard".to_owned(),
            "kbd-b".to_owned(),
        ];
        assert_eq!(
            cmd::lua_bind(&devs),
            "eval local t = __claude_cu_abort or {}\n__claude_cu_abort = t\nt[#t + 1] = hl.bind(\"EsCaPe\", hl.dsp.event(\"claude-cu-abort\"), { non_consuming = true, dont_inhibit = true, description = \"claude-cu-abort\", device = { inclusive = true, list = { \"at-translated-set-2-keyboard\", \"kbd-b\" } } })"
        );
        // The exact form tested live on Hyprland 0.56.2 against an expired
        // bind (replied ok, no crash).
        assert_eq!(
            cmd::lua_unbind(),
            "eval for _, b in ipairs(__claude_cu_abort or {}) do if tostring(b) ~= \"HL.Keybind(expired)\" then pcall(function() b:remove() end) end end\n__claude_cu_abort = nil"
        );
        assert_eq!(
            cmd::legacy_bind_devices(&devs),
            "keyword bindnpkd , EsCaPe, at-translated-set-2-keyboard kbd-b, claude-cu-abort, event, claude-cu-abort"
        );
        assert_eq!(
            cmd::legacy_bind_global(),
            [
                "keyword bindnpd , EsCaPe, claude-cu-abort, event, claude-cu-abort".to_owned(),
                "keyword bindnd , EsCaPe, claude-cu-abort, event, claude-cu-abort".to_owned()
            ]
        );
        assert_eq!(cmd::legacy_unbind(), "keyword unbind , EsCaPe");
        assert_eq!(cmd::abort_event(), "custom>>claude-cu-abort");
        assert_eq!(
            cmd::lua_str("a\"b\\c\n\u{e9}"),
            "\"a\\\"b\\\\c\\010\\195\\169\""
        );
    }

    #[test]
    fn dialect_detection() {
        // `j/status` at 0.55+ (SystemInfo::getStatus); 0.54: unknown request.
        assert_eq!(
            parse_status("\n{\n    \"configProvider\": \"lua\",\n    \"backend\": \"drm\"\n}\n")
                .unwrap(),
            Dialect::Lua
        );
        assert_eq!(
            parse_status("{\"configProvider\": \"hyprlang\", \"backend\": \"drm\"}").unwrap(),
            Dialect::Hyprlang
        );
        assert_eq!(
            parse_status("unknown request").unwrap(),
            Dialect::OldHyprlang
        );
        // Anything else is not guessed at.
        assert!(parse_status("").is_err());
        assert!(parse_status("{\"configProvider\": \"error\"}").is_err());
        assert!(parse_status("permission denied").is_err());
        assert!(cmd::is_invalid_flag("bind: invalid flag"));
        assert!(!cmd::is_invalid_flag("ok"));
        for v in [Variant::Lua, Variant::LegacyDevices, Variant::LegacyGlobal] {
            assert_eq!(Variant::decode(v.encode()), Some(v));
        }
        assert!(!Variant::LegacyGlobal.synthetic_safe());
        assert!(Variant::Lua.synthetic_safe() && Variant::LegacyDevices.synthetic_safe());
    }

    #[test]
    fn device_classification() {
        // `j/devices` keyboards: shape from Hyprland 0.57 devicesRequest.
        let d = json!({"mice":[],"keyboards":[
            {"address":"0x1","name":"at-translated-set-2-keyboard","main":true},
            {"address":"0x2","name":"hl-virtual-keyboard","main":false},
            {"address":"0x3","name":"hl-virtual-keyboard-wlroots-bridge-1","main":false},
            {"address":"0x4","name":"ydotoold-virtual-device","main":false},
            {"address":"0x5","name":"keyd-virtual-keyboard","main":false},
            {"address":"0x6","name":"power-button","main":false},
            {"address":"0x7","name":"at-translated-set-2-keyboard","main":false}
        ]});
        assert_eq!(
            physical_names(&d),
            vec![
                "at-translated-set-2-keyboard",
                "keyd-virtual-keyboard",
                "power-button"
            ]
        );
        assert!(cmd::legacy_safe_name("logitech-g915-(wireless)"));
        assert!(cmd::legacy_safe_name("kbd;\"odd\""));
        assert!(!cmd::legacy_safe_name("!neg"));
        assert!(!cmd::legacy_safe_name("a b"));
        assert!(!cmd::legacy_safe_name("a,b"));
    }

    #[test]
    fn user_bind_detection() {
        // `j/binds` entries: 0.57 prints submap_universal as a quoted string.
        let binds = json!([
            {"modmask":64,"submap":"","submap_universal":"false","key":"ESCAPE","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"exec","arg":"power-menu"},
            {"modmask":0,"submap":"resize","submap_universal":"false","key":"escape","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"submap","arg":"reset"},
            {"modmask":0,"submap":"","submap_universal":"false","key":"EsCaPe","keycode":0,"catch_all":false,"mouse":false,"description":"claude-cu-abort","dispatcher":"event","arg":"claude-cu-abort"},
            {"modmask":0,"submap":"","submap_universal":"false","key":"EsCaPe","keycode":0,"catch_all":false,"mouse":false,"description":"claude-cu-abort","dispatcher":"HL.Dispatcher","arg":"12"}
        ]);
        assert!(user_escape_binds(&binds).is_empty());
        assert!(ours_present(&binds));
        let mut user = binds.as_array().unwrap().clone();
        user.push(json!({"modmask":0,"submap":"","submap_universal":"false","key":"escape","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"exec","arg":"foo"}));
        assert_eq!(
            user_escape_binds(&Value::Array(user)),
            vec!["Escape -> exec foo"]
        );
        // Lua binds: the dispatcher is `__lua <ref>`; the description names it.
        let lua = json!([{"modmask":0,"submap":"","submap_universal":"false","key":"Escape","keycode":0,"catch_all":false,"mouse":false,"description":"Close notifications","dispatcher":"__lua","arg":"17"}]);
        assert_eq!(
            user_escape_binds(&lua),
            vec!["Escape -> \"Close notifications\""]
        );
        let universal = json!([{"modmask":0,"submap":"resize","submap_universal":"true","key":"","keycode":9,"catch_all":false,"mouse":false,"description":"","dispatcher":"submap","arg":"reset"}]);
        assert_eq!(user_escape_binds(&universal).len(), 1);
        assert!(!ours_present(&json!([])));
    }

    #[test]
    fn ignore_mods_text_binds() {
        // Plain `binds` at 0.57 (bindsRequest FORMAT_NORMAL).
        let text = "bind\n\tflags: \n\tmodmask: 64\n\tsubmap: \n\tkey: ESCAPE\n\tkeycode: 0\n\tcatchall: false\n\tdescription: \n\tdispatcher: exec\n\targ: menu\n\n\
bind\n\tflags: non_consuming, ignore_mods\n\tmodmask: 64\n\tsubmap: \n\tkey: Escape\n\tkeycode: 0\n\tcatchall: false\n\tdescription: \n\tdispatcher: exec\n\targ: ign\n\n\
bind\n\tflags: ignore_mods\n\tmodmask: 0\n\tsubmap: resize\n\tkey: escape\n\tkeycode: 0\n\tcatchall: false\n\tdescription: \n\tdispatcher: submap\n\targ: reset\n\n";
        assert_eq!(
            ignore_mods_escape_binds(text),
            vec!["Escape (ignore_mods) -> exec ign"]
        );
        // 0.54/0.55 text has no flags line: nothing detectable.
        assert!(ignore_mods_escape_binds("bindn\n\tmodmask: 0\n\tkey: escape\n\n").is_empty());
    }

    #[test]
    fn socket2_lines() {
        let mut p = b"workspace>>1\ncustom>>claude-cu-abort\r\nconfigrel".to_vec();
        assert_eq!(
            take_lines(&mut p),
            vec!["workspace>>1", "custom>>claude-cu-abort"]
        );
        assert_eq!(p, b"configrel");
        p.extend_from_slice(b"oaded>>\n");
        assert_eq!(take_lines(&mut p), vec!["configreloaded>>"]);
        assert!(p.is_empty());
    }
}
