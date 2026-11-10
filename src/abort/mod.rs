//! `abort-watch` (optional part of the bridge CLI contract): "the user pressed Escape, stop
//! Computer Use", on sway and Hyprland.
//!
//! The protocol, signals, parent-death and exactly-once cleanup live in the
//! shared `cu-abort-watch` crate; this module only supplies the compositor
//! half ([`cu_abort_watch::Backend`]):
//!
//! - sway ([`sway`]): one `bindsym --input-device=<physical keyboard>
//!   --no-repeat --inhibited Escape nop claude-cu-abort` per physical
//!   keyboard, in the `default` mode, reported through the IPC `binding`
//!   event. The bridge's own virtual keyboard is never bound, so an Escape
//!   the model injects cannot abort (`syntheticSafe: true`).
//! - Hyprland ([`hypr`]): an Escape bind scoped to the physical keyboards that
//!   runs the `event` dispatcher (`custom>>claude-cu-abort` on socket2). Lua
//!   config (0.55+): `hl.bind` over `eval`; hyprlang: `keyword bindkd`, or a
//!   global `bindd` (`syntheticSafe: false`, suspend = unbind) where the
//!   per-device flag is missing.
//! - Anything else (niri, river, labwc, ...): an `error` event and exit 2.
//!
//! Binds are user-global, so several Claude profiles share them through
//! [`cu_abort_watch::SharedBinding`]: first holder binds, last unbinds, and a
//! record file in the holder directory lists what is bound so a crashed
//! holder's binds are cleared by the next first holder.

pub mod hypr;
pub mod sway;
pub mod swayconf;

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cu_abort_watch::{
    AbortSink, AbortWatchDoctor, Backend, BoxError, Mechanism, Ready, SharedBinding, binding_dir,
};
use serde::{Deserialize, Serialize};

/// The string both compositors carry back to us (sway `nop` argument,
/// Hyprland `event` payload / bind description).
pub const MARKER: &str = "claude-cu-abort";

/// Presses closer than this to the previous one are one press (key repeat
/// where `--no-repeat` is missing, duplicate binds left by a crash).
pub const DEBOUNCE: Duration = Duration::from_millis(1000);

/// Budget for the doctor probe (short request connections only).
const DOCTOR_BUDGET: Duration = Duration::from_millis(1500);

pub(crate) fn log(msg: impl std::fmt::Display) {
    eprintln!("wlroots-bridge abort-watch: {msg}");
}

/// Chained debounce: an event within [`DEBOUNCE`] of the previous event
/// (accepted or not) is dropped, so a held key collapses into one press.
#[derive(Default)]
pub struct Debounce {
    last: Mutex<Option<Instant>>,
}

impl Debounce {
    pub fn accept_at(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        let ok = last.is_none_or(|t| now.saturating_duration_since(t) >= DEBOUNCE);
        *last = Some(now);
        ok
    }

    pub fn abort(&self, sink: &AbortSink) {
        if self.accept_at(Instant::now()) {
            sink.abort();
        }
    }
}

/// What is bound right now, shared by every holder of one mechanism. Written
/// before a bind (intent), removed by the last holder after its unbind; a
/// record that exists while no holder is alive means a holder crashed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Backend-specific form of the bind (see `sway::Variant`, `hypr::Variant`).
    pub variant: String,
    /// Devices (sway identifiers / Hyprland device names) bound.
    pub devices: Vec<String>,
}

/// File name inside the holder directory (holder files are numeric).
pub const RECORD_FILE: &str = "bound.json";

impl Record {
    pub fn read(dir: &Path) -> Option<Self> {
        let raw = std::fs::read(dir.join(RECORD_FILE)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let tmp = dir.join(format!(".{RECORD_FILE}.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(io::Error::other)?)?;
        std::fs::rename(tmp, dir.join(RECORD_FILE))
    }

    pub fn remove(dir: &Path) {
        let _ = std::fs::remove_file(dir.join(RECORD_FILE));
    }

    /// Whether a record file exists at all (even an unparsable one).
    pub fn exists(dir: &Path) -> bool {
        dir.join(RECORD_FILE).exists()
    }
}

// ------------------------------------------------------ holder sets

/// FNV-1a 64, hex: a stable short name for an instance key.
pub fn fnv64_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// One sway instance = one IPC socket (canonical path).
pub fn sway_instance_key(sock: &Path) -> String {
    let p = sock.canonicalize().unwrap_or_else(|_| sock.to_path_buf());
    format!("i{}", fnv64_hex(p.as_os_str().as_bytes()))
}

/// One Hyprland instance = one instance signature (used as is when it is a
/// plain file name, else hashed).
pub fn hypr_instance_key(sig: &str) -> String {
    let plain = !sig.is_empty()
        && sig.len() <= 128
        && !sig.starts_with('.')
        && !sig.ends_with("-suspended")
        && sig
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    if plain {
        sig.to_owned()
    } else {
        format!("h{}", fnv64_hex(sig.as_bytes()))
    }
}

/// The holder set of ONE compositor instance:
/// `$XDG_RUNTIME_DIR/claude-cu-esc/<mechanism>/<instance key>`. Two sways
/// (or Hyprlands) of one user never share binds, so they never share holders.
/// The crate creates and checks `claude-cu-esc` and `<mechanism>`; the
/// instance directory gets the same ownership checks from `in_dir`.
pub fn instance_set(mechanism: Mechanism, key: &str) -> io::Result<SharedBinding> {
    let base = SharedBinding::for_mechanism(mechanism)?;
    SharedBinding::in_dir(base.dir().join(key))
}

/// The instance directory path without creating anything (doctor).
pub fn instance_path(mechanism: Mechanism, key: &str) -> Option<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())?;
    Some(binding_dir(&rt, mechanism).join(key))
}

/// A crashed watcher left something behind: a record or holder files, and
/// no holder process is alive. Read-only (taking the lock would prune the
/// holder files the next first holder needs). A reused pid counts as alive.
pub fn is_stale(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    let (mut holders, mut live) = (0, 0);
    for e in rd.flatten() {
        if let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) {
            holders += 1;
            if Path::new("/proc").join(pid.to_string()).exists() {
                live += 1;
            }
        }
    }
    (Record::exists(dir) || holders > 0) && live == 0
}

/// Which backend the running session gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    Sway(std::path::PathBuf),
    Hyprland(hypr::Paths),
    /// No usable runtime binding API; the full reason.
    Other(String),
}

/// Pick the backend from the HOST compositor, sway first. `SWAYSOCK` only
/// counts when it accepts a connection: a stale one is inherited by sessions
/// started from sway (a nested Hyprland, a second VT).
pub fn detect() -> Session {
    if let Some(p) = crate::ax::sway_socket()
        && std::os::unix::net::UnixStream::connect(&p).is_ok()
    {
        return Session::Sway(p);
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|s| !s.is_empty()) {
        return match hypr::Paths::from_env() {
            Some(p) => Session::Hyprland(p),
            None => Session::Other("Hyprland IPC socket not found".into()),
        };
    }
    if crate::ax::sway_socket().is_some() {
        return Session::Other("SWAYSOCK is set but sway does not answer".into());
    }
    Session::Other(unsupported_reason(&other_compositor_name()))
}

fn other_compositor_name() -> String {
    if std::env::var_os("NIRI_SOCKET").is_some_and(|s| !s.is_empty()) {
        return "niri".into();
    }
    std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .and_then(|d| d.split(':').next().map(str::to_ascii_lowercase))
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "this compositor".into())
}

/// The message both `abort-watch` and `doctor` give for compositors without
/// a runtime binding API.
pub fn unsupported_reason(name: &str) -> String {
    format!("no runtime binding API on {name}")
}

/// Test hook (debug builds only; release binaries ignore it): make `arm`
/// fail right after the bind, to prove the rollback leaves nothing behind.
pub(crate) fn injected_failure_after_bind() -> Result<(), BoxError> {
    if cfg!(debug_assertions) && std::env::var_os("WLROOTS_BRIDGE_TEST_FAIL_AFTER_BIND").is_some() {
        return Err("injected failure after bind (test hook)".into());
    }
    Ok(())
}

/// Arming always fails: nothing is bound, `run` exits 2.
struct Unsupported(String);

impl Backend for Unsupported {
    fn arm(&mut self, _sink: AbortSink) -> Result<Ready, BoxError> {
        Err(self.0.clone().into())
    }
    fn disarm(&mut self) {}
}

/// `wlroots-bridge abort-watch`.
pub fn run() -> ExitCode {
    match detect() {
        Session::Sway(sock) => cu_abort_watch::run(sway::SwayBackend::new(sock, None)),
        Session::Hyprland(paths) => cu_abort_watch::run(hypr::HyprBackend::new(paths, None)),
        Session::Other(reason) => cu_abort_watch::run(Unsupported(reason)),
    }
}

/// What a backend probe found for `doctor`.
pub struct Probe {
    /// Physical keyboards `abort-watch` could not bind right now.
    pub skipped: Vec<String>,
    /// Whether arming would take Escape away from applications: sway `true`
    /// (no non-consuming bindsym), Hyprland `false` (non-consuming bind),
    /// `None` when the version cannot be told from the compositor.
    pub consumes: Option<bool>,
}

/// `doctor.abortWatch`: `{mechanism, available, reason}` (the crate's shape)
/// plus `stale` (a crashed watcher left binds: run `abort-watch` with stdin
/// at EOF to clear them), `skippedKeyboards` and `consumes` (see [`Probe`]). Short request connections
/// only, never a subscription, bounded by [`DOCTOR_BUDGET`].
pub fn doctor() -> serde_json::Value {
    let session = detect();
    let (mechanism, key) = match &session {
        Session::Sway(sock) => (Mechanism::Sway, sway_instance_key(sock)),
        Session::Hyprland(p) => (Mechanism::Hyprland, hypr_instance_key(&p.signature)),
        Session::Other(name) => {
            let mut v = AbortWatchDoctor::unavailable(None, name.clone()).to_value();
            v["stale"] = false.into();
            v["skippedKeyboards"] = serde_json::json!([]);
            v["consumes"] = serde_json::Value::Null;
            return v;
        }
    };
    let stale = instance_path(mechanism, &key).is_some_and(|d| is_stale(&d));
    let probe = move || {
        Some(match session {
            Session::Sway(sock) => sway::probe(&sock),
            Session::Hyprland(paths) => hypr::probe(&paths),
            Session::Other(_) => unreachable!(),
        })
    };
    // What arming does to Escape, as far as the mechanism alone tells.
    let mut consumes = match mechanism {
        Mechanism::Sway => Some(true),
        _ => None,
    };
    let (base, skipped) = match crate::ax::with_deadline(DOCTOR_BUDGET, probe) {
        Some(Ok(p)) => {
            consumes = p.consumes;
            (AbortWatchDoctor::available(mechanism), p.skipped)
        }
        Some(Err(reason)) => (
            AbortWatchDoctor::unavailable(Some(mechanism), reason),
            Vec::new(),
        ),
        None => (
            AbortWatchDoctor::unavailable(
                Some(mechanism),
                format!(
                    "compositor IPC did not answer within {} ms",
                    DOCTOR_BUDGET.as_millis()
                ),
            ),
            Vec::new(),
        ),
    };
    let mut v = base.to_value();
    v["stale"] = stale.into();
    v["skippedKeyboards"] = serde_json::json!(skipped);
    v["consumes"] = consumes.into();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_collapses_repeats() {
        let d = Debounce::default();
        let t0 = Instant::now();
        assert!(d.accept_at(t0));
        // Key repeat: 600 ms delay, then every 40 ms - all one press.
        assert!(!d.accept_at(t0 + Duration::from_millis(600)));
        assert!(!d.accept_at(t0 + Duration::from_millis(640)));
        assert!(!d.accept_at(t0 + Duration::from_millis(1500)));
        // A real second press after a pause.
        assert!(d.accept_at(t0 + Duration::from_millis(2600)));
    }

    #[test]
    fn record_roundtrip() {
        let d = std::env::temp_dir().join(format!("wlroots-bridge-record-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(Record::read(&d), None);
        let r = Record {
            variant: "x".into(),
            devices: vec!["a".into(), "b".into()],
        };
        r.write(&d).unwrap();
        assert!(Record::exists(&d));
        assert_eq!(Record::read(&d), Some(r));
        Record::remove(&d);
        assert!(!Record::exists(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn instance_keys() {
        assert_eq!(hypr_instance_key("abc_123.4-x"), "abc_123.4-x");
        for odd in ["", "../x", "a/b", ".hidden", "x-suspended"] {
            let k = hypr_instance_key(odd);
            assert!(k.starts_with('h') && k.len() == 17, "{odd} -> {k}");
        }
        let a = sway_instance_key(Path::new("/run/user/1/sway-ipc.1.2.sock"));
        let b = sway_instance_key(Path::new("/run/user/1/sway-ipc.1.3.sock"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 17);
        assert_eq!(fnv64_hex(b""), "cbf29ce484222325");
    }

    #[test]
    fn stale_detection() {
        let d = std::env::temp_dir().join(format!("wlroots-bridge-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(!is_stale(&d), "empty");
        std::fs::write(d.join(".lock"), "").unwrap();
        assert!(!is_stale(&d), "lock only");
        std::fs::write(d.join("999999999"), "1\n").unwrap();
        assert!(is_stale(&d), "dead holder");
        std::fs::write(d.join(std::process::id().to_string()), "1\n").unwrap();
        assert!(!is_stale(&d), "a live holder");
        std::fs::remove_file(d.join(std::process::id().to_string())).unwrap();
        std::fs::remove_file(d.join("999999999")).unwrap();
        Record::default().write(&d).unwrap();
        assert!(is_stale(&d), "record without holder");
        let _ = std::fs::remove_dir_all(&d);
        assert!(!is_stale(&d), "missing dir");
    }

    #[test]
    fn unsupported_message() {
        assert_eq!(unsupported_reason("niri"), "no runtime binding API on niri");
    }
}
