//! sway backend of `abort-watch`.
//!
//! Arm: for every physical keyboard (see [`is_physical_keyboard`])
//! `mode default bindsym --input-device=<identifier> --no-repeat --inhibited
//! Escape nop claude-cu-abort`; subscribe to `binding`, `input`, `mode`,
//! `workspace` and `shutdown`. A `binding` event whose command is
//! `nop claude-cu-abort` is the user's Escape. A device-scoped bind never
//! fires for another device, so the bridge's own `wlr_virtual_keyboard_v1`
//! (a separate input device) cannot trigger it: `syntheticSafe: true`.
//!
//! sway has no non-consuming `bindsym`: while armed, Escape on a physical
//! keyboard is consumed by sway (the focused app does not get it).
//!
//! - New keyboard (`input` `added`): bound too.
//! - `reload` (`workspace` event `reload`): sway rebuilt its config and
//!   dropped every runtime bind: the user-binding check runs again, then
//!   everything is re-bound (an identical `bindsym` overwrites, so every
//!   holder doing it is harmless). When the reload brought a user Escape
//!   binding, the record is emptied first (our binds are already gone) and
//!   the watcher stops, so no unbind can hit the user's binding.
//! - `mode default` prefix: the bind lands in the `default` mode from any
//!   mode, without a `mode` event. An identifier that needs quoting cannot
//!   use it (sway keeps quotes literally inside `mode <name> ...`): those
//!   keyboards are bound with the plain quoted form while sway is in the
//!   `default` mode, and on the next `mode` change to `default` otherwise.
//! - sway 1.12 runs a press twice when a non-default mode's Escape binding
//!   switches back to `default` (the stock `resize` mode): mode change, the
//!   user's binding, then ours. A binding event of ours right after a mode
//!   change or another Escape binding ([`QUIRK_WINDOW`]) is dropped.
//! - The user's own bare Escape binding in the `default` mode (read from the
//!   config, see [`super::swayconf`]) makes arming fail: a device-scoped
//!   bind would shadow it on that keyboard. A user binding with exactly our
//!   signature is never unbound (sway's binding comparison ignores
//!   `--no-repeat`, so our unbind would remove it).

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use cu_abort_watch::{AbortSink, Backend, BoxError, Mechanism, Ready, SharedBinding};
use serde_json::{Value, json};

use super::swayconf::{self, Env, Finding};
use super::{Debounce, MARKER, Probe, Record, log};
use crate::ax::{IPC_TIMEOUT, i3_frame};

// ------------------------------------------------------------- i3-ipc

pub const RUN_COMMAND: u32 = 0;
pub const SUBSCRIBE: u32 = 2;
pub const GET_CONFIG: u32 = 9;
pub const GET_BINDING_STATE: u32 = 12;
pub const GET_INPUTS: u32 = 100;
pub const EVENT_BIT: u32 = 0x8000_0000;
pub const EV_WORKSPACE: u32 = EVENT_BIT;
pub const EV_MODE: u32 = EVENT_BIT | 2;
pub const EV_BINDING: u32 = EVENT_BIT | 5;
pub const EV_SHUTDOWN: u32 = EVENT_BIT | 6;
pub const EV_INPUT: u32 = EVENT_BIT | 0x15;

/// How long `disarm` waits for the event thread before releasing anyway.
const THREAD_JOIN: Duration = Duration::from_secs(2);
/// A binding event of ours this soon after a mode change or another Escape
/// binding is sway re-running the same press (see module docs).
pub const QUIRK_WINDOW: Duration = Duration::from_millis(100);
/// Upper bound for reading the sway config (GET_CONFIG + includes).
const CONFIG_DEADLINE: Duration = Duration::from_secs(2);

fn connect(path: &Path) -> std::io::Result<UnixStream> {
    let s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(IPC_TIMEOUT))?;
    s.set_write_timeout(Some(IPC_TIMEOUT))?;
    Ok(s)
}

fn send(s: &mut UnixStream, ty: u32, payload: &[u8]) -> std::io::Result<()> {
    s.write_all(&i3_frame(ty, payload))
}

/// One message (reply or event): (type, JSON body).
fn recv(s: &mut UnixStream) -> Result<(u32, Value)> {
    let mut head = [0u8; 14];
    s.read_exact(&mut head)?;
    if &head[..6] != b"i3-ipc" {
        bail!("bad i3-ipc magic");
    }
    let len = u32::from_ne_bytes(head[6..10].try_into()?) as usize;
    let ty = u32::from_ne_bytes(head[10..14].try_into()?);
    if len > 64 << 20 {
        bail!("i3-ipc message too large");
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    Ok((
        ty,
        serde_json::from_slice(&body).context("i3-ipc body is not JSON")?,
    ))
}

/// One request on a fresh connection (1.5 s timeouts each way).
fn request(path: &Path, ty: u32, payload: &str) -> Result<Value> {
    let mut s = connect(path).context("connect SWAYSOCK")?;
    send(&mut s, ty, payload.as_bytes())?;
    let (rty, v) = recv(&mut s)?;
    if rty != ty {
        bail!("unexpected i3-ipc reply type {rty} to {ty}");
    }
    Ok(v)
}

/// `RUN_COMMAND`: one result per `;`-separated command.
fn run_commands(path: &Path, cmds: &str) -> Result<Vec<std::result::Result<(), String>>> {
    let v = request(path, RUN_COMMAND, cmds)?;
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("RUN_COMMAND reply is not a list"))?;
    Ok(list
        .iter()
        .map(|r| {
            if r.get("success").and_then(Value::as_bool) == Some(true) {
                Ok(())
            } else {
                Err(r
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_owned())
            }
        })
        .collect())
}

/// The pid of the process at the other end (sway), via `SO_PEERCRED`.
fn peer_pid(s: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid fd, `cred` and `len` describe a writable ucred buffer.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

// ------------------------------------------------- sway command strings

/// How the bind command is spelled; fixed by the first successful bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    /// `mode default ` prefix (bind into the default mode from any mode).
    pub mode_prefix: bool,
    /// `--no-repeat` (sway 1.8+); without it, key repeat is debounced.
    pub no_repeat: bool,
}

impl Variant {
    pub const PREFERRED: Variant = Variant {
        mode_prefix: true,
        no_repeat: true,
    };

    pub fn encode(self) -> String {
        format!(
            "sway:mode={},norepeat={}",
            u8::from(self.mode_prefix),
            u8::from(self.no_repeat)
        )
    }

    pub fn decode(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("sway:mode=")?;
        let (m, r) = rest.split_once(",norepeat=")?;
        Some(Self {
            mode_prefix: m == "1",
            no_repeat: r == "1",
        })
    }
}

/// Every sway command this backend sends.
pub mod cmd {
    use super::{MARKER, Variant, is_safe_identifier};

    /// What the binding runs, and what the `binding` event reports back.
    pub fn abort_command() -> String {
        format!("nop {MARKER}")
    }

    /// `--input-device=<id>` as sway must see it: bare for plain
    /// identifiers, else one double-quoted argument (sway 1.12 strips the
    /// quotes but does NOT unescape backslashes, so identifiers with `"` or
    /// `\` are never bound, see [`super::is_bindable_identifier`]).
    pub fn device_arg(id: &str) -> String {
        if is_safe_identifier(id) {
            format!("--input-device={id}")
        } else {
            format!("\"--input-device={id}\"")
        }
    }

    /// Whether `id` can only be (un)bound while sway is in the default mode:
    /// the quoted form cannot carry the `mode default` prefix.
    pub fn needs_default_mode(id: &str, v: Variant) -> bool {
        !v.mode_prefix || !is_safe_identifier(id)
    }

    fn spec(verb: &str, id: &str, v: Variant) -> String {
        format!(
            "{}{verb} {}{} --inhibited Escape",
            if needs_default_mode(id, v) {
                ""
            } else {
                "mode default "
            },
            device_arg(id),
            if v.no_repeat { " --no-repeat" } else { "" }
        )
    }

    /// `[mode default ]bindsym --input-device=<id> [--no-repeat] --inhibited Escape nop claude-cu-abort`
    pub fn bind(id: &str, v: Variant) -> String {
        format!("{} {}", spec("bindsym", id, v), abort_command())
    }

    /// The exact counterpart of [`bind`] (same mode, device and flags).
    pub fn unbind(id: &str, v: Variant) -> String {
        spec("unbindsym", id, v)
    }

    pub fn batch(ids: &[String], v: Variant, f: fn(&str, Variant) -> String) -> String {
        ids.iter().map(|id| f(id, v)).collect::<Vec<_>>().join("; ")
    }
}

// ------------------------------------------------- devices and events

/// Input devices that inject synthetic keys and must never be bound:
/// - `wlr_virtual_keyboard_v1` (every `zwp_virtual_keyboard_v1` client: this
///   bridge, wtype, ...); `virtual keyboard` on older wlroots;
/// - ydotool's uinput device (`ydotoold virtual device`).
///
/// Remappers that carry the user's real keystrokes through uinput (keyd,
/// kanata, kmonad) ARE the physical keyboard from sway's point of view and
/// stay bound.
pub fn is_injector(input: &Value) -> bool {
    let name = input.get("name").and_then(Value::as_str).unwrap_or("");
    let id = input
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or("");
    name == "wlr_virtual_keyboard_v1"
        || name == "virtual keyboard"
        || id.ends_with(":wlr_virtual_keyboard_v1")
        || id == "0:0:virtual_keyboard"
        || name.to_ascii_lowercase().contains("ydotool")
}

/// A keyboard a person types on: `type` `keyboard` and not an injector.
pub fn is_physical_keyboard(input: &Value) -> bool {
    input.get("type").and_then(Value::as_str) == Some("keyboard") && !is_injector(input)
}

/// An identifier sway takes unquoted (and inside `mode default ...`). sway
/// already turns spaces and non-printable bytes into `_`; identifiers with
/// characters its command parser treats specially need the quoted form.
pub fn is_safe_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_:.-+/()@&!?=<>|~%*^[]".contains(&b))
}

/// An identifier we put into a command at all (quoted when needed). A `"`
/// or `\` cannot be expressed: sway keeps backslash escapes literally inside
/// quotes (tested on 1.12), so the bind would name another device.
pub fn is_bindable_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id
            .bytes()
            .any(|b| b.is_ascii_control() || b == b'"' || b == b'\\')
}

/// Physical keyboards whose identifier can never be bound.
pub fn unbindable_identifiers(inputs: &Value) -> Vec<String> {
    let mut out = BTreeSet::new();
    for i in inputs.as_array().map(Vec::as_slice).unwrap_or_default() {
        if let (true, Some(id)) = (
            is_physical_keyboard(i),
            i.get("identifier").and_then(Value::as_str),
        ) && !is_bindable_identifier(id)
        {
            out.insert(id.to_owned());
        }
    }
    out.into_iter().collect()
}

/// The identifiers to bind, from a `GET_INPUTS` reply (sorted, unique).
pub fn physical_identifiers(inputs: &Value) -> Vec<String> {
    let mut out = BTreeSet::new();
    for i in inputs.as_array().map(Vec::as_slice).unwrap_or_default() {
        if !is_physical_keyboard(i) {
            continue;
        }
        let Some(id) = i.get("identifier").and_then(Value::as_str) else {
            continue;
        };
        if is_bindable_identifier(id) {
            out.insert(id.to_owned());
        } else {
            log(format!("skipping keyboard with unusable identifier {id:?}"));
        }
    }
    out.into_iter().collect()
}

/// What an event means for us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwayEvent {
    /// Our binding ran: the user pressed Escape on a physical keyboard.
    Abort,
    /// Another binding ran for Escape (the user's own, any mode).
    OtherEscape,
    /// The binding mode changed to this name.
    Mode(String),
    /// A physical keyboard appeared.
    KeyboardAdded(String),
    /// Config reloaded: runtime binds are gone.
    Reload,
    Shutdown,
    Other,
}

pub fn parse_event(ty: u32, body: &Value) -> SwayEvent {
    let change = body.get("change").and_then(Value::as_str);
    match ty {
        EV_BINDING if change == Some("run") => {
            let command = body.pointer("/binding/command").and_then(Value::as_str);
            if command.is_some_and(|c| c.trim() == cmd::abort_command()) {
                return SwayEvent::Abort;
            }
            let sym = body.pointer("/binding/symbol").and_then(Value::as_str);
            let code = body.pointer("/binding/input_code").and_then(Value::as_u64);
            if sym.is_some_and(|s| s.eq_ignore_ascii_case("escape")) || code == Some(9) {
                SwayEvent::OtherEscape
            } else {
                SwayEvent::Other
            }
        }
        EV_MODE => SwayEvent::Mode(change.unwrap_or("").to_owned()),
        EV_INPUT if change == Some("added") => match body.get("input") {
            Some(i) if is_physical_keyboard(i) => {
                match i.get("identifier").and_then(Value::as_str) {
                    Some(id) if is_bindable_identifier(id) => {
                        SwayEvent::KeyboardAdded(id.to_owned())
                    }
                    _ => SwayEvent::Other,
                }
            }
            _ => SwayEvent::Other,
        },
        EV_WORKSPACE if change == Some("reload") => SwayEvent::Reload,
        EV_SHUTDOWN => SwayEvent::Shutdown,
        _ => SwayEvent::Other,
    }
}

/// O1: drop our binding event when the event just before it (within
/// [`QUIRK_WINDOW`]) was a mode change or another Escape binding.
pub fn is_quirk_rerun(prev: Option<&(SwayEvent, Instant)>, now: Instant) -> bool {
    prev.is_some_and(|(e, t)| {
        matches!(e, SwayEvent::Mode(_) | SwayEvent::OtherEscape)
            && now.saturating_duration_since(*t) < QUIRK_WINDOW
    })
}

// ----------------------------------------------------------------- core

/// Bind/unbind logic shared by `arm`, the event thread and `disarm`.
#[derive(Clone)]
struct Core {
    sock: PathBuf,
    set: SharedBinding,
}

impl Core {
    fn dir(&self) -> &Path {
        self.set.dir()
    }

    fn physical_ids(&self) -> Result<Vec<String>> {
        Ok(physical_identifiers(&request(&self.sock, GET_INPUTS, "")?))
    }

    /// The user's bare Escape bindings in the default mode.
    fn findings(&self) -> Result<Vec<Finding>> {
        user_escape_bindings(&self.sock)
    }

    fn mode(&self) -> Result<String> {
        Ok(request(&self.sock, GET_BINDING_STATE, "")?
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned())
    }

    /// Bind `ids`. With `known` unset, learn the spelling this sway accepts
    /// on the first id. Returns the spelling and the ids actually bound: an
    /// id that can only be bound in the default mode is skipped while sway
    /// is in another mode. Leaves nothing bound when it fails.
    fn bind(&self, ids: &[String], known: Option<Variant>) -> Result<(Variant, Vec<String>)> {
        let mut v = known.unwrap_or(Variant::PREFERRED);
        let mut mode: Option<String> = None;
        let mut in_default = |core: &Core| -> Result<bool> {
            if mode.is_none() {
                mode = Some(core.mode()?);
            }
            Ok(mode.as_deref() == Some("default"))
        };
        let mut todo: Vec<String> = Vec::new();
        for id in ids {
            if cmd::needs_default_mode(id, v) && !in_default(self)? {
                log(format!(
                    "keyboard {id} waits for the default mode to be bound"
                ));
                continue;
            }
            todo.push(id.clone());
        }
        let Some(first) = todo.first().cloned() else {
            return Ok((v, Vec::new()));
        };
        loop {
            let r = run_commands(&self.sock, &cmd::bind(&first, v))?;
            match r.into_iter().next() {
                Some(Ok(())) => break,
                Some(Err(e)) if v.no_repeat && e.contains("--no-repeat") && known.is_none() => {
                    v.no_repeat = false;
                }
                Some(Err(e))
                    if v.mode_prefix && !cmd::needs_default_mode(&first, v) && known.is_none() =>
                {
                    // An older sway that refuses mode subcommands at runtime:
                    // a plain bindsym lands in the CURRENT mode, so only use
                    // it while that is `default`.
                    if !in_default(self)? {
                        bail!(
                            "sway refused `mode default bindsym` ({e}) and is not in the default mode"
                        );
                    }
                    v.mode_prefix = false;
                }
                Some(Err(e)) => bail!("sway refused the Escape bind for {first}: {e}"),
                None => bail!("sway sent no result for the Escape bind"),
            }
        }
        // The spelling may have changed: re-check the mode rule for the rest.
        let mut rest: Vec<String> = Vec::new();
        for id in &todo[1..] {
            if cmd::needs_default_mode(id, v) && !in_default(self)? {
                log(format!(
                    "keyboard {id} waits for the default mode to be bound"
                ));
                continue;
            }
            rest.push(id.clone());
        }
        if !rest.is_empty() {
            let r = run_commands(&self.sock, &cmd::batch(&rest, v, cmd::bind))?;
            if let Some(Err(e)) = r.iter().find(|x| x.is_err()) {
                let e = e.clone();
                let mut done = vec![first.clone()];
                done.extend(rest.iter().cloned());
                self.unbind(&done, Some(v), &[]);
                bail!("sway refused an Escape bind: {e}");
            }
        }
        let mut bound = vec![first];
        bound.extend(rest);
        Ok((v, bound))
    }

    /// Remove our binds for `ids`. Never touches an id where the user has a
    /// binding with our exact signature (`user`). Without a known spelling
    /// both mode forms are tried. Returns the ids that could not be unbound
    /// now (quoted ones while sway is not in the default mode).
    fn unbind(&self, ids: &[String], v: Option<Variant>, user: &[Finding]) -> Vec<String> {
        let ids: Vec<String> = ids
            .iter()
            .filter(|id| {
                let mine = user.iter().any(|f| f.same_signature_as_ours(id));
                if mine {
                    log(format!(
                        "not unbinding {id}: the user binds Escape with the same signature"
                    ));
                }
                !mine
            })
            .cloned()
            .collect();
        if ids.is_empty() {
            return Vec::new();
        }
        let variants: Vec<Variant> = match v {
            Some(v) => vec![v],
            None => vec![
                Variant::PREFERRED,
                Variant {
                    mode_prefix: false,
                    no_repeat: true,
                },
            ],
        };
        let in_default = self.mode().map(|m| m == "default").unwrap_or(false);
        let mut left = Vec::new();
        for (n, v) in variants.iter().enumerate() {
            let (now, later): (Vec<String>, Vec<String>) = ids
                .iter()
                .cloned()
                .partition(|id| !cmd::needs_default_mode(id, *v) || in_default);
            if n == 0 {
                left = later;
            }
            if now.is_empty() {
                continue;
            }
            if let Err(e) = run_commands(&self.sock, &cmd::batch(&now, *v, cmd::unbind)) {
                log(format!("unbind failed: {e:#}"));
            }
        }
        left
    }

    /// Binds left by a holder that died: everything in the record plus every
    /// current keyboard (its record may predate a hot-plugged device), except
    /// the user's same-signature bindings.
    fn clear_stale(&self, user: &[Finding]) {
        let rec = Record::read(self.dir());
        let mut ids: BTreeSet<String> = rec.iter().flat_map(|r| r.devices.clone()).collect();
        if let Ok(now) = self.physical_ids() {
            ids.extend(now);
        }
        let ids: Vec<String> = ids.into_iter().collect();
        log(format!(
            "clearing binds left by a crashed holder ({} devices)",
            ids.len()
        ));
        let v = rec.as_ref().and_then(|r| Variant::decode(&r.variant));
        let left = self.unbind(&ids, v, user);
        self.finish_record(left, v);
    }

    /// After an unbind: no record when everything is gone, else a record of
    /// what is left (the next first holder or a heal clears it).
    fn finish_record(&self, left: Vec<String>, v: Option<Variant>) {
        if left.is_empty() {
            Record::remove(self.dir());
        } else {
            log(format!(
                "{} binds need the default mode to be removed; kept for the next cleanup",
                left.len()
            ));
            let _ = Record {
                variant: v.map(Variant::encode).unwrap_or_default(),
                devices: left,
            }
            .write(self.dir());
        }
    }

    /// Join the holder set; the first holder clears stale binds, checks for
    /// the user's Escape binding and binds every physical keyboard.
    fn join(&self) -> Result<(), BoxError> {
        let mut lock = self.set.lock()?;
        if !lock.is_first() {
            lock.register()?;
            return Ok(());
        }
        let user = self.findings()?;
        // Stale binds first (they would shadow the user's own Escape), never
        // touching a user binding with our signature.
        if lock.pruned() > 0 || Record::exists(self.dir()) {
            self.clear_stale(&user);
        }
        if let Some(c) = user.first() {
            return Err(format!(
                "Escape is already bound in sway's default mode ({c}); not overriding the user's binding"
            )
            .into());
        }
        let inputs = request(&self.sock, GET_INPUTS, "")?;
        let ids = physical_identifiers(&inputs);
        let never = unbindable_identifiers(&inputs);
        if ids.is_empty() && !never.is_empty() {
            return Err(format!(
                "no physical keyboard can be bound: identifiers {never:?} contain characters sway commands cannot express"
            )
            .into());
        }
        if ids.is_empty() {
            log("no physical keyboard yet: binding when one appears");
        }
        // Intent first: a crash between here and the end leaves a record.
        Record {
            variant: String::new(),
            devices: ids.clone(),
        }
        .write(self.dir())?;
        lock.register()?;
        let undo = |lock: &mut cu_abort_watch::HolderLock<'_>| {
            Record::remove(self.dir());
            let _ = lock.unregister();
        };
        match self.bind(&ids, None) {
            Ok((_, bound)) if !ids.is_empty() && bound.is_empty() => {
                undo(&mut lock);
                Err(format!(
                    "none of the {} physical keyboards could be bound now ({}): their identifiers need sway's default mode",
                    ids.len(),
                    ids.join(", ")
                )
                .into())
            }
            Ok((v, bound)) => {
                if bound.len() < ids.len() {
                    log(format!(
                        "{} of {} keyboards bound; the rest when sway returns to the default mode",
                        bound.len(),
                        ids.len()
                    ));
                }
                Record {
                    // Only a bind that ran proves the spelling.
                    variant: if bound.is_empty() {
                        String::new()
                    } else {
                        v.encode()
                    },
                    devices: bound,
                }
                .write(self.dir())?;
                Ok(())
            }
            Err(e) => {
                undo(&mut lock);
                Err(format!("{e:#}").into())
            }
        }
    }

    /// Event thread: bind physical keyboards the record does not list yet
    /// (hot-plug, or back in the default mode). Once across holders.
    fn bind_missing(&self, ids: Vec<String>) -> Result<()> {
        let lock = self.set.lock()?;
        if !lock.is_registered() {
            return Ok(()); // disarming
        }
        let mut rec = Record::read(self.dir()).unwrap_or_default();
        let new: Vec<String> = ids
            .into_iter()
            .filter(|id| !rec.devices.contains(id))
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        let (v, bound) = self.bind(&new, Variant::decode(&rec.variant))?;
        if bound.is_empty() {
            return Ok(());
        }
        rec.variant = v.encode();
        log(format!("bound keyboards {bound:?}"));
        rec.devices.extend(bound);
        rec.write(self.dir())?;
        Ok(())
    }

    /// Event thread, after a sway `reload`: check the user's bindings again
    /// and re-bind everything. `Err` = the watcher must stop.
    fn after_reload(&self) -> Result<()> {
        let lock = self.set.lock()?;
        if !lock.is_registered() {
            return Ok(());
        }
        let user = self.findings()?;
        let mut rec = Record::read(self.dir()).unwrap_or_default();
        if let Some(c) = user.first() {
            // The reload dropped every bind of ours: nothing is left to
            // unbind, and an unbind could hit the user's new binding.
            rec.devices.clear();
            rec.write(self.dir())?;
            bail!(
                "sway reload added an Escape binding in the default mode ({c}); Escape abort stopped"
            );
        }
        let mut ids: BTreeSet<String> = rec.devices.iter().cloned().collect();
        ids.extend(self.physical_ids()?);
        let ids: Vec<String> = ids.into_iter().collect();
        let (v, bound) = self.bind(&ids, Variant::decode(&rec.variant))?;
        if !bound.is_empty() {
            rec.variant = v.encode();
        }
        rec.devices = bound;
        rec.write(self.dir())?;
        log("re-bound after sway reload");
        Ok(())
    }

    /// The last holder's unbind (inside `release`).
    fn unbind_all(&self) {
        let Some(rec) = Record::read(self.dir()) else {
            Record::remove(self.dir());
            return;
        };
        let user = self.findings().unwrap_or_default();
        let v = Variant::decode(&rec.variant);
        let left = self.unbind(&rec.devices, v, &user);
        self.finish_record(left, v);
    }
}

/// The user's bare Escape bindings in sway's default mode, bounded in time
/// (GET_CONFIG has its own timeouts; reading includes is local file I/O).
fn user_escape_bindings(sock: &Path) -> Result<Vec<Finding>> {
    let sock = sock.to_path_buf();
    match crate::ax::with_deadline(CONFIG_DEADLINE, move || {
        Some(read_user_escape_bindings(&sock))
    }) {
        Some(r) => r,
        None => bail!(
            "reading the sway config took longer than {} ms",
            CONFIG_DEADLINE.as_millis()
        ),
    }
}

fn read_user_escape_bindings(sock: &Path) -> Result<Vec<Finding>> {
    let mut s = connect(sock).context("connect SWAYSOCK")?;
    let pid = peer_pid(&s);
    send(&mut s, GET_CONFIG, b"")?;
    let (_, v) = recv(&mut s)?;
    let text = v
        .get("config")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GET_CONFIG reply has no config"))?;
    let env = pid.and_then(Env::of_pid).unwrap_or_else(Env::current);
    let main = swayconf::main_config_path(pid, text, &env);
    Ok(swayconf::scan(text, main.as_deref(), &env))
}

/// `doctor`: the socket answers and no user Escape binding is in the way.
/// `skipped`: keyboards that cannot be bound until sway is in the default mode.
pub fn probe(sock: &Path) -> std::result::Result<Probe, String> {
    let inputs = request(sock, GET_INPUTS, "").map_err(|e| format!("sway IPC: {e:#}"))?;
    match user_escape_bindings(sock) {
        Ok(f) if f.is_empty() => {}
        Ok(f) => {
            return Err(format!(
                "Escape is already bound in sway's default mode ({})",
                f[0]
            ));
        }
        Err(e) => return Err(format!("cannot read the sway config: {e:#}")),
    }
    let mode = request(sock, GET_BINDING_STATE, "")
        .ok()
        .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "default".into());
    let mut skipped = unbindable_identifiers(&inputs);
    if mode != "default" {
        skipped.extend(
            physical_identifiers(&inputs)
                .into_iter()
                .filter(|id| !is_safe_identifier(id)),
        );
    }
    Ok(Probe {
        skipped,
        // sway has no non-consuming bindsym.
        consumes: Some(true),
    })
}

// -------------------------------------------------------------- backend

pub struct SwayBackend {
    sock: PathBuf,
    /// Holder directory override (tests); `None` = this sway instance's
    /// `$XDG_RUNTIME_DIR/claude-cu-esc/sway/<instance>`.
    shared_dir: Option<PathBuf>,
    armed: Option<Armed>,
}

struct Armed {
    core: Core,
    stop: Arc<AtomicBool>,
    sub: UnixStream,
    done: mpsc::Receiver<()>,
}

impl SwayBackend {
    pub fn new(sock: PathBuf, shared_dir: Option<PathBuf>) -> Self {
        Self {
            sock,
            shared_dir,
            armed: None,
        }
    }

    fn subscribe(&self) -> Result<UnixStream> {
        let mut s = connect(&self.sock).context("connect SWAYSOCK")?;
        let events = json!(["binding", "input", "mode", "workspace", "shutdown"]).to_string();
        send(&mut s, SUBSCRIBE, events.as_bytes())?;
        let (ty, v) = recv(&mut s)?;
        if ty != SUBSCRIBE || v.get("success").and_then(Value::as_bool) != Some(true) {
            bail!("sway refused the event subscription: {v}");
        }
        // Events can be minutes apart; the thread blocks until disarm shuts
        // the socket down.
        s.set_read_timeout(None)?;
        Ok(s)
    }
}

fn event_loop(mut sub: UnixStream, core: Core, sink: AbortSink, stop: Arc<AtomicBool>) {
    let debounce = Debounce::default();
    let mut prev: Option<(SwayEvent, Instant)> = None;
    loop {
        let (ty, body) = match recv(&mut sub) {
            Ok(m) => m,
            Err(e) => {
                if !stop.load(Ordering::SeqCst) {
                    sink.fail(format!("sway IPC closed: {e:#}"));
                }
                return;
            }
        };
        let now = Instant::now();
        let ev = parse_event(ty, &body);
        match &ev {
            SwayEvent::Abort => {
                if is_quirk_rerun(prev.as_ref(), now) {
                    log("ignoring our binding re-run right after a mode change");
                } else {
                    debounce.abort(&sink);
                }
            }
            SwayEvent::KeyboardAdded(id) => {
                if let Err(e) = core.bind_missing(vec![id.clone()]) {
                    log(format!("could not bind new keyboard {id}: {e:#}"));
                }
            }
            SwayEvent::Mode(m) if m == "default" => {
                match core.physical_ids().and_then(|ids| core.bind_missing(ids)) {
                    Ok(()) => {}
                    Err(e) => log(format!("could not bind waiting keyboards: {e:#}")),
                }
            }
            SwayEvent::Reload => {
                if let Err(e) = core.after_reload() {
                    sink.fail(format!("{e:#}"));
                    return;
                }
            }
            SwayEvent::Shutdown => {
                sink.fail("sway is shutting down");
                return;
            }
            SwayEvent::Mode(_) | SwayEvent::OtherEscape | SwayEvent::Other => {}
        }
        prev = Some((ev, now));
    }
}

impl Backend for SwayBackend {
    fn arm(&mut self, sink: AbortSink) -> std::result::Result<Ready, BoxError> {
        let set = match &self.shared_dir {
            Some(d) => SharedBinding::in_dir(d)?,
            None => super::instance_set(Mechanism::Sway, &super::sway_instance_key(&self.sock))?,
        };
        let core = Core {
            sock: self.sock.clone(),
            set,
        };
        // Subscribe before binding so no hot-plugged keyboard is missed.
        // Every fallible step without a side effect comes before `join`.
        let sub = self.subscribe()?;
        let thread_sub = sub.try_clone()?;
        if let Err(e) = core.join() {
            let _ = sub.shutdown(std::net::Shutdown::Both);
            return Err(e);
        }
        // From here on an error must undo the join (holder file, record,
        // binds): `arm` leaves nothing behind on Err.
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, done) = mpsc::channel();
        let (c, s) = (core.clone(), stop.clone());
        let spawned = super::injected_failure_after_bind().and_then(|()| {
            std::thread::Builder::new()
                .name("sway-events".into())
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
            stop,
            sub,
            done,
        });
        Ok(Ready {
            mechanism: Mechanism::Sway,
            synthetic_safe: true,
        })
    }

    fn disarm(&mut self) {
        let Some(a) = self.armed.take() else { return };
        a.stop.store(true, Ordering::SeqCst);
        let _ = a.sub.shutdown(std::net::Shutdown::Both);
        let _ = a.done.recv_timeout(THREAD_JOIN);
        let core = a.core.clone();
        if let Err(e) = a.core.set.release(|| core.unbind_all()) {
            log(format!("release: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_strings() {
        let id = "1:1:AT_Translated_Set_2_keyboard";
        assert_eq!(
            cmd::bind(id, Variant::PREFERRED),
            "mode default bindsym --input-device=1:1:AT_Translated_Set_2_keyboard --no-repeat --inhibited Escape nop claude-cu-abort"
        );
        assert_eq!(
            cmd::unbind(id, Variant::PREFERRED),
            "mode default unbindsym --input-device=1:1:AT_Translated_Set_2_keyboard --no-repeat --inhibited Escape"
        );
        let old = Variant {
            mode_prefix: false,
            no_repeat: false,
        };
        assert_eq!(
            cmd::bind(id, old),
            "bindsym --input-device=1:1:AT_Translated_Set_2_keyboard --inhibited Escape nop claude-cu-abort"
        );
        assert_eq!(
            cmd::unbind(id, old),
            "unbindsym --input-device=1:1:AT_Translated_Set_2_keyboard --inhibited Escape"
        );
        assert_eq!(
            cmd::batch(&["a".into(), "b".into()], old, cmd::unbind),
            "unbindsym --input-device=a --inhibited Escape; unbindsym --input-device=b --inhibited Escape"
        );
        // Identifiers that need quoting: plain (no mode prefix) form, quoted.
        assert_eq!(
            cmd::bind("1:1:a;b,c", Variant::PREFERRED),
            r#"bindsym "--input-device=1:1:a;b,c" --no-repeat --inhibited Escape nop claude-cu-abort"#
        );
        assert!(!is_bindable_identifier("1:1:a\"b"));
        assert!(!is_bindable_identifier("1:1:a\\b"));
        assert!(is_bindable_identifier("1:1:a;b,c$#{}'"));
        assert!(cmd::needs_default_mode("1:1:a,b", Variant::PREFERRED));
        assert!(!cmd::needs_default_mode(id, Variant::PREFERRED));
        assert!(cmd::needs_default_mode(id, old));
    }

    #[test]
    fn variant_roundtrip() {
        for m in [false, true] {
            for r in [false, true] {
                let v = Variant {
                    mode_prefix: m,
                    no_repeat: r,
                };
                assert_eq!(Variant::decode(&v.encode()), Some(v));
            }
        }
        assert_eq!(Variant::decode("hypr:lua"), None);
    }

    #[test]
    fn device_classification() {
        // Shapes from sway 1.12 GET_INPUTS / input events.
        let inputs = json!([
            {"identifier":"1:1:AT_Translated_Set_2_keyboard","name":"AT Translated Set 2 keyboard","type":"keyboard","libinput":{}},
            {"identifier":"0:0:wlr_virtual_keyboard_v1","name":"wlr_virtual_keyboard_v1","type":"keyboard"},
            {"identifier":"0:0:virtual_keyboard","name":"virtual keyboard","type":"keyboard"},
            {"identifier":"4660:22136:ydotoold_virtual_device","name":"ydotoold virtual device","type":"keyboard","libinput":{}},
            {"identifier":"4660:22136:ydotoold_virtual_device","name":"ydotoold virtual device","type":"pointer","libinput":{}},
            {"identifier":"0:1:keyd_virtual_keyboard","name":"keyd virtual keyboard","type":"keyboard","libinput":{}},
            {"identifier":"0:0:wayland-keyboard-seat0","name":"wayland-keyboard-seat0","type":"keyboard"},
            {"identifier":"1739:52619:SYNA8004:00_06CB:CD8B_Touchpad","name":"SYNA8004:00 06CB:CD8B Touchpad","type":"touchpad"},
            {"identifier":"1:1:AT_Translated_Set_2_keyboard","name":"AT Translated Set 2 keyboard","type":"keyboard"},
            {"identifier":"6:6:odd;name","name":"odd;name","type":"keyboard"},
            {"identifier":"7:7:ctl\u{1}x","name":"ctl","type":"keyboard"},
            {"identifier":"8:8:q\"uote","name":"q\"uote","type":"keyboard"}
        ]);
        assert_eq!(
            unbindable_identifiers(&inputs),
            vec!["7:7:ctl\u{1}x", "8:8:q\"uote"]
        );
        assert_eq!(
            physical_identifiers(&inputs),
            vec![
                "0:0:wayland-keyboard-seat0",
                "0:1:keyd_virtual_keyboard",
                "1:1:AT_Translated_Set_2_keyboard",
                "6:6:odd;name",
            ]
        );
        assert!(is_safe_identifier("1133:49970:Logitech_G915_(Wireless)"));
        for bad in ["a;b", "a,b", "a b", "\"a", "$x", "a{", "a#b", "a\\b", ""] {
            assert!(!is_safe_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn event_parsing() {
        // From a live sway 1.12 `swaymsg -t subscribe -m '["binding","input","mode"]'`.
        let ours = json!({"change":"run","binding":{"command":"nop claude-cu-abort","event_state_mask":[],"input_codes":[],"input_code":0,"symbols":["Escape"],"symbol":"Escape","input_type":"keyboard"}});
        assert_eq!(parse_event(EV_BINDING, &ours), SwayEvent::Abort);
        let user = json!({"change":"run","binding":{"command":"mode default","symbol":"Escape","input_code":0}});
        assert_eq!(parse_event(EV_BINDING, &user), SwayEvent::OtherEscape);
        let other = json!({"change":"run","binding":{"command":"nop something-else","symbol":"a"}});
        assert_eq!(parse_event(EV_BINDING, &other), SwayEvent::Other);
        assert_eq!(
            parse_event(EV_MODE, &json!({"change":"default","pango_markup":false})),
            SwayEvent::Mode("default".into())
        );
        let added = json!({"change":"added","input":{"identifier":"0:0:wayland-keyboard-seat0","name":"wayland-keyboard-seat0","type":"keyboard"}});
        assert_eq!(
            parse_event(EV_INPUT, &added),
            SwayEvent::KeyboardAdded("0:0:wayland-keyboard-seat0".into())
        );
        let virt = json!({"change":"added","input":{"identifier":"0:0:wlr_virtual_keyboard_v1","name":"wlr_virtual_keyboard_v1","type":"keyboard"}});
        assert_eq!(parse_event(EV_INPUT, &virt), SwayEvent::Other);
        let removed = json!({"change":"removed","input":{"identifier":"x","type":"keyboard"}});
        assert_eq!(parse_event(EV_INPUT, &removed), SwayEvent::Other);
        let reload = json!({"change":"reload","old":null,"current":null});
        assert_eq!(parse_event(EV_WORKSPACE, &reload), SwayEvent::Reload);
        assert_eq!(
            parse_event(EV_SHUTDOWN, &json!({"change":"exit"})),
            SwayEvent::Shutdown
        );
        assert_eq!(parse_event(EV_INPUT, &ours), SwayEvent::Other);
    }

    #[test]
    fn quirk_filter() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        // Measured order on 1.12: mode change, user binding, ours.
        let after_user = (SwayEvent::OtherEscape, t);
        assert!(is_quirk_rerun(Some(&after_user), ms(2)));
        let after_mode = (SwayEvent::Mode("default".into()), t);
        assert!(is_quirk_rerun(Some(&after_mode), ms(5)));
        // A real press later, or after unrelated events: kept.
        assert!(!is_quirk_rerun(Some(&after_user), ms(150)));
        assert!(!is_quirk_rerun(Some(&(SwayEvent::Other, t)), ms(1)));
        assert!(!is_quirk_rerun(Some(&(SwayEvent::Abort, t)), ms(1)));
        assert!(!is_quirk_rerun(None, ms(1)));
    }
}
