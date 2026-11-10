//! Finds the user's own bare-Escape bindings in sway's `default` mode.
//!
//! sway has no IPC request that lists bindings. `GET_CONFIG` returns the text
//! of the main config file sway loaded (includes are NOT expanded), so this
//! module re-reads the config the way sway does: line continuations, comments,
//! `{ }` blocks (`mode "x" { ... }`, `bindsym { ... }`), `set $var` variables,
//! and `include` (sway variables, `~`, `$ENV`, `*`/`?` globs, relative to the
//! including file). It keeps every `bindsym` / `bindcode` that lands in the
//! `default` mode with a bare `Escape` (or keycode 9) and no modifier, and
//! drops the ones a later `unbindsym` / `unbindcode` removes.
//!
//! Not visible here: bindings a user added at runtime with `swaymsg bindsym`
//! and includes sway could read but this process cannot. Those are the
//! documented limits of the "never shadow a user's Escape" check.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One user binding that would conflict with ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// File the binding came from (`<GET_CONFIG>` for the main text when its
    /// path is unknown), 1-based line.
    pub file: String,
    pub line: usize,
    pub text: String,
    /// `bindcode` (true) or `bindsym`.
    pub code: bool,
    /// `--input-device=` value, `*` when absent.
    pub device: String,
    /// The conflict flags sway compares (`--release`, `--inhibited`, ...).
    pub flags: Vec<String>,
}

impl Finding {
    /// Whether this is a binding sway treats as the SAME binding as ours on
    /// `id` (`binding_key_compare`: same input, same conflict flags, same
    /// keysym list; `--no-repeat` is not compared). Our unbind would remove
    /// it, so it must never be unbound.
    pub fn same_signature_as_ours(&self, id: &str) -> bool {
        !self.code && self.device == id && self.flags == ["--inhibited"]
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.text)
    }
}

/// Environment used for `~` / `$VAR` in include paths (sway's own, when
/// readable, so a different `$HOME` or `$XDG_CONFIG_HOME` resolves alike).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub vars: HashMap<String, String>,
}

impl Env {
    pub fn current() -> Self {
        Self {
            vars: std::env::vars().collect(),
        }
    }

    /// The environment of process `pid` (`/proc/<pid>/environ`), or `None`.
    pub fn of_pid(pid: u32) -> Option<Self> {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let vars = raw
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let kv = std::str::from_utf8(kv).ok()?;
                let (k, v) = kv.split_once('=')?;
                Some((k.to_owned(), v.to_owned()))
            })
            .collect();
        Some(Self { vars })
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.vars
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// The main config file sway loaded: `-c`/`--config` from its command line,
/// else sway's search order. A candidate whose content equals `text` wins;
/// otherwise the first that exists (only used to resolve relative includes).
pub fn main_config_path(pid: Option<u32>, text: &str, env: &Env) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(pid) = pid
        && let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline"))
    {
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let mut it = args.iter().skip(1);
        while let Some(a) = it.next() {
            let p = if a == "-c" || a == "--config" {
                it.next().cloned()
            } else {
                a.strip_prefix("--config=").map(str::to_owned)
            };
            if let Some(p) = p {
                let p = PathBuf::from(p);
                let p = if p.is_relative() {
                    std::fs::read_link(format!("/proc/{pid}/cwd"))
                        .map(|cwd| cwd.join(&p))
                        .unwrap_or(p)
                } else {
                    p
                };
                candidates.push(p);
            }
        }
    }
    if let Some(home) = env.get("HOME") {
        let xdg = env
            .get("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(home).join(".config"));
        candidates.push(Path::new(home).join(".sway/config"));
        candidates.push(xdg.join("sway/config"));
        candidates.push(Path::new(home).join(".i3/config"));
        candidates.push(xdg.join("i3/config"));
    }
    candidates.push(PathBuf::from("/etc/sway/config"));
    candidates.push(PathBuf::from("/etc/i3/config"));

    if let Some(p) = candidates
        .iter()
        .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t == text))
    {
        return Some(p.clone());
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Scan the config sway reported. `main_path` is where `text` came from
/// (for relative includes and messages).
pub fn scan(text: &str, main_path: Option<&Path>, env: &Env) -> Vec<Finding> {
    let mut st = State {
        env,
        vars: Vec::new(),
        binds: Vec::new(),
        visited: HashSet::new(),
    };
    let label = main_path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<GET_CONFIG>".to_owned());
    if let Some(p) = main_path
        && let Ok(c) = p.canonicalize()
    {
        st.visited.insert(c);
    }
    st.file(text, &label, main_path.and_then(Path::parent), 0);
    st.binds.into_iter().map(|(_, f)| f).collect()
}

/// What two bindings must share for one to replace or remove the other
/// (sway's `binding_key_compare`: input device, conflict flags, keys).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sig {
    code: bool,
    key: String,
    device: String,
    flags: BTreeSet<String>,
}

const CONFLICT_FLAGS: &[&str] = &[
    "--release",
    "--locked",
    "--inhibited",
    "--border",
    "--whole-window",
    "--exclude-titlebar",
];

const MAX_INCLUDE_DEPTH: usize = 16;

struct State<'a> {
    env: &'a Env,
    /// `$name` -> value, in definition order (later wins).
    vars: Vec<(String, String)>,
    binds: Vec<(Sig, Finding)>,
    visited: HashSet<PathBuf>,
}

/// Physical lines -> (first line number, logical line): joins `\`
/// continuations, trims, drops blanks and `#` comments.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut acc = String::new();
    let mut start = 0;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if acc.is_empty() {
            start = i + 1;
            if line.starts_with('#') {
                continue;
            }
        }
        if let Some(head) = line.strip_suffix('\\') {
            acc.push_str(head);
            acc.push(' ');
            continue;
        }
        acc.push_str(line);
        let l = acc.trim().to_owned();
        acc.clear();
        if !l.is_empty() {
            out.push((start, l));
        }
    }
    let l = acc.trim();
    if !l.is_empty() {
        out.push((start, l.to_owned()));
    }
    out
}

/// sway's argument splitting: whitespace, `"..."` / `'...'` quoting
/// (quotes removed), backslash escapes inside double quotes.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut have = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                have = true;
            }
            None if c.is_whitespace() => {
                if have || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            None => cur.push(c),
        }
    }
    if have || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// sway's var replacement: each `$` takes the longest defined variable name
/// that starts there.
fn replace_vars(s: &str, vars: &[(String, String)]) -> String {
    if vars.is_empty() || !s.contains('$') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with('$') {
            let best = vars
                .iter()
                .rev()
                .filter(|(n, _)| rest.starts_with(n.as_str()))
                .max_by_key(|(n, _)| n.len());
            if let Some((n, v)) = best {
                out.push_str(v);
                i += n.len();
                continue;
            }
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// `~` and `$VAR` / `${VAR}` against `env` (the wordexp part of `include`).
fn expand_env(s: &str, env: &Env) -> String {
    let s = match s.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", env.get("HOME").unwrap_or("~"))
        }
        _ => s.to_owned(),
    };
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let (name, end) = if b.get(i + 1) == Some(&b'{') {
                match s[i + 2..].find('}') {
                    Some(e) => (&s[i + 2..i + 2 + e], i + 3 + e),
                    None => ("", i + 1),
                }
            } else {
                let n = s[i + 1..]
                    .bytes()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
                    .count();
                (&s[i + 1..i + 1 + n], i + 1 + n)
            };
            if !name.is_empty() {
                out.push_str(env.get(name).unwrap_or(""));
                i = end;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// `0xff1b` (XK_Escape as a hex keysym, which xkb accepts as a name).
fn is_escape_hex(key: &str) -> bool {
    key.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"))
        && u32::from_str_radix(&key[2..], 16) == Ok(0xff1b)
}

/// Shell-style match of one path component (`*`, `?`; no `[...]`).
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    match (pat.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            glob_match(&pat[1..], name) || (!name.is_empty() && glob_match(pat, &name[1..]))
        }
        (Some(b'?'), Some(_)) => glob_match(&pat[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => glob_match(&pat[1..], &name[1..]),
        _ => false,
    }
}

/// Expand `*` / `?` per component, sorted like wordexp. Hidden entries only
/// match a pattern that starts with `.`.
fn glob(path: &Path) -> Vec<PathBuf> {
    let mut acc = vec![PathBuf::new()];
    for comp in path.components() {
        let c = comp.as_os_str().to_string_lossy();
        if !c.contains(['*', '?']) {
            for p in &mut acc {
                p.push(comp.as_os_str());
            }
            continue;
        }
        let mut next = Vec::new();
        for base in &acc {
            let dir = if base.as_os_str().is_empty() {
                Path::new(".")
            } else {
                base.as_path()
            };
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut names: Vec<String> = rd
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.') || c.starts_with('.'))
                .filter(|n| glob_match(c.as_bytes(), n.as_bytes()))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|n| base.join(n)));
        }
        acc = next;
    }
    acc
}

impl State<'_> {
    fn file(&mut self, text: &str, label: &str, dir: Option<&Path>, depth: usize) {
        // Block prefixes, innermost last (already joined with the outer ones).
        let mut blocks: Vec<String> = Vec::new();
        let mut last: Option<String> = None;
        for (lineno, line) in logical_lines(text) {
            if line == "}" {
                blocks.pop();
                continue;
            }
            let joined = |l: &str, blocks: &[String]| match blocks.last() {
                Some(p) => format!("{p} {l}"),
                None => l.to_owned(),
            };
            if line == "{" {
                // `mode foo` on the previous line opens the block.
                if let Some(prev) = last.take() {
                    blocks.push(prev);
                }
                continue;
            }
            if let Some(head) = line.strip_suffix('{') {
                let head = head.trim();
                blocks.push(joined(head, &blocks));
                continue;
            }
            let cmd = joined(&line, &blocks);
            last = Some(cmd.clone());
            self.command(&cmd, label, lineno, dir, depth);
        }
    }

    fn command(&mut self, cmd: &str, label: &str, line: usize, dir: Option<&Path>, depth: usize) {
        let is_set = cmd
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("set"));
        let cmd = if is_set {
            cmd.to_owned()
        } else {
            replace_vars(cmd, &self.vars)
        };
        let args = split_args(&cmd);
        let Some(head) = args.first() else { return };
        match head.to_ascii_lowercase().as_str() {
            "set" => {
                if let (Some(name), true) = (args.get(1), args.len() > 2) {
                    let name = if name.starts_with('$') {
                        name.clone()
                    } else {
                        format!("${name}")
                    };
                    let value = replace_vars(&args[2..].join(" "), &self.vars);
                    self.vars.retain(|(n, _)| n != &name);
                    self.vars.push((name, value));
                }
            }
            "include" => {
                if depth >= MAX_INCLUDE_DEPTH || args.len() < 2 {
                    return;
                }
                let raw = expand_env(&args[1..].join(" "), self.env);
                let p = PathBuf::from(&raw);
                let p = match (p.is_relative(), dir) {
                    (true, Some(d)) => d.join(p),
                    _ => p,
                };
                for f in glob(&p) {
                    let Ok(canon) = f.canonicalize() else {
                        continue;
                    };
                    if !self.visited.insert(canon) {
                        continue;
                    }
                    // Only regular files: a FIFO or device would block the
                    // read (and with it, arming).
                    if !std::fs::metadata(&f).is_ok_and(|m| m.is_file()) {
                        eprintln!(
                            "wlroots-bridge abort-watch: skipping sway include {} (not a regular file)",
                            f.display()
                        );
                        continue;
                    }
                    match std::fs::read_to_string(&f) {
                        Ok(t) => {
                            let lbl = f.display().to_string();
                            self.file(&t, &lbl, f.parent(), depth + 1);
                        }
                        Err(e) => eprintln!(
                            "wlroots-bridge abort-watch: cannot read sway include {}: {e}",
                            f.display()
                        ),
                    }
                }
            }
            "mode" => {
                let mut i = 1;
                if args.get(i).is_some_and(|a| a == "--pango_markup") {
                    i += 1;
                }
                if let Some(name) = args.get(i)
                    && args.len() > i + 1
                {
                    let mode = name.clone();
                    self.binding(&mode, &args[i + 1..], label, line, &cmd);
                }
            }
            _ => self.binding("default", &args, label, line, &cmd),
        }
    }

    fn binding(&mut self, mode: &str, args: &[String], label: &str, line: usize, text: &str) {
        let Some(head) = args.first() else { return };
        let (code, bind) = match head.to_ascii_lowercase().as_str() {
            "bindsym" => (false, true),
            "bindcode" => (true, true),
            "unbindsym" => (false, false),
            "unbindcode" => (true, false),
            _ => return,
        };
        if mode != "default" {
            return;
        }
        let mut device = "*".to_owned();
        let mut flags = BTreeSet::new();
        let mut rest = args[1..].iter();
        let combo = loop {
            let Some(a) = rest.next() else { return };
            if let Some(d) = a.strip_prefix("--input-device=") {
                device = d.to_owned();
            } else if a.starts_with("--") {
                if CONFLICT_FLAGS.contains(&a.as_str()) {
                    flags.insert(a.clone());
                }
            } else {
                break a;
            }
        };
        let parts: Vec<&str> = combo.split('+').collect();
        if parts.len() != 1 {
            return; // a modifier or a chord: not a bare key
        }
        let key = parts[0];
        let escape = if code {
            key == "9"
        } else {
            key.eq_ignore_ascii_case("escape") || is_escape_hex(key)
        };
        if !escape {
            return;
        }
        let sig = Sig {
            code,
            key: key.to_ascii_lowercase(),
            device,
            flags,
        };
        // A binding with the same signature replaces the earlier one; an
        // unbind removes it.
        self.binds.retain(|(s, _)| s != &sig);
        if bind {
            let f = Finding {
                file: label.to_owned(),
                line,
                text: text.to_owned(),
                code: sig.code,
                device: sig.device.clone(),
                flags: sig.flags.iter().cloned().collect(),
            };
            self.binds.push((sig, f));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        let mut e = Env::default();
        e.vars.insert("HOME".into(), "/home/u".into());
        e
    }

    fn found(text: &str) -> Vec<String> {
        scan(text, None, &env())
            .into_iter()
            .map(|f| f.text)
            .collect()
    }

    #[test]
    fn bare_escape_in_default_mode() {
        assert_eq!(found("bindsym Escape exec foo\n").len(), 1);
        assert_eq!(found("bindsym escape exec foo\n").len(), 1);
        assert_eq!(found("bindcode 9 exec foo\n").len(), 1);
        assert_eq!(
            found("bindsym --release --to-code Escape exec foo\n").len(),
            1
        );
        assert_eq!(
            found("bindsym --input-device=1:1:kbd Escape exec foo\n").len(),
            1
        );
        assert_eq!(found("mode default bindsym Escape exec foo\n").len(), 1);
        assert_eq!(found("bindsym 0xff1b exec foo\n").len(), 1);
        assert_eq!(found("bindsym 0XFF1B exec foo\n").len(), 1);
        assert!(found("bindsym 0xff1c exec foo\n").is_empty());
        assert_eq!(
            found("mode \"default\" {\n  bindsym Escape exec foo\n}\n").len(),
            1
        );
    }

    #[test]
    fn not_conflicts() {
        assert!(found("bindsym Mod4+Escape exec foo\n").is_empty());
        assert!(found("bindsym Shift+escape exec foo\n").is_empty());
        assert!(found("bindcode 10 exec foo\n").is_empty());
        assert!(found("bindsym Escape+a exec foo\n").is_empty());
        assert!(found("# bindsym Escape exec foo\n").is_empty());
        // The stock sway config: Escape only in the resize mode.
        let stock = "set $mod Mod4\nmode \"resize\" {\n    bindsym Left resize shrink width 10px\n    bindsym Return mode \"default\"\n    bindsym Escape mode \"default\"\n}\nbindsym $mod+r mode \"resize\"\n";
        assert!(found(stock).is_empty());
        assert!(found("mode resize bindsym Escape mode default\n").is_empty());
    }

    #[test]
    fn variables_blocks_and_continuations() {
        assert_eq!(found("set $esc Escape\nbindsym $esc exec foo\n").len(), 1);
        assert!(found("set $mod Mod4\nbindsym $mod+Escape exec foo\n").is_empty());
        // Longest variable name wins.
        assert!(
            found("set $m Mod1\nset $mod Mod4\nset $e Escape\nbindsym $mod+$e exec x\n").is_empty()
        );
        assert_eq!(
            found("bindsym {\n  Escape exec foo\n  a exec bar\n}\n").len(),
            1
        );
        assert_eq!(found("bindsym \\\n   Escape exec foo\n").len(), 1);
        let nested = "mode \"default\" {\n bindsym --release {\n  Escape exec foo\n }\n}\n";
        assert_eq!(found(nested).len(), 1);
        assert_eq!(
            found("mode default\n{\nbindsym Escape exec x\n}\n").len(),
            1
        );
    }

    #[test]
    fn same_signature_as_ours() {
        let id = "1:1:kbd";
        let f = |t: &str| scan(t, None, &env()).remove(0);
        assert!(
            f("bindsym --input-device=1:1:kbd --inhibited Escape exec x\n")
                .same_signature_as_ours(id)
        );
        assert!(
            f("bindsym --input-device=1:1:kbd --no-repeat --inhibited Escape exec x\n")
                .same_signature_as_ours(id)
        );
        assert!(!f("bindsym --input-device=1:1:kbd Escape exec x\n").same_signature_as_ours(id));
        assert!(!f("bindsym --inhibited Escape exec x\n").same_signature_as_ours(id));
        assert!(
            !f("bindsym --input-device=1:1:kbd --inhibited --release Escape exec x\n")
                .same_signature_as_ours(id)
        );
        assert!(
            !f("bindcode --input-device=1:1:kbd --inhibited 9 exec x\n").same_signature_as_ours(id)
        );
    }

    #[test]
    fn fifo_include_does_not_block() {
        let d = tmpdir("fifo");
        let fifo = d.join("pipe.conf");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(d.join("x.conf"), "bindsym Escape exec x\n").unwrap();
        let main = d.join("config");
        let f = scan("include *.conf\n", Some(&main), &env());
        assert_eq!(f.len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unbind_removes_same_signature_only() {
        assert!(found("bindsym Escape exec foo\nunbindsym Escape\n").is_empty());
        assert_eq!(
            found("bindsym Escape exec foo\nunbindsym --release Escape\n").len(),
            1
        );
        // Rebinding replaces: one finding, the later text.
        let f = found("bindsym Escape exec a\nbindsym Escape exec b\n");
        assert_eq!(f, vec!["bindsym Escape exec b".to_owned()]);
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "wlroots-bridge-swayconf-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn includes_globs_relative_and_env() {
        let d = tmpdir("inc");
        std::fs::create_dir_all(d.join("config.d")).unwrap();
        std::fs::write(d.join("config.d/10-a.conf"), "bindsym a exec a\n").unwrap();
        std::fs::write(
            d.join("config.d/50-esc.conf"),
            "set $k Escape\nbindsym $k exec user\n",
        )
        .unwrap();
        std::fs::write(d.join("config.d/.hidden"), "bindsym Escape exec no\n").unwrap();
        std::fs::write(d.join("other"), "include config.d/*.conf\n").unwrap();
        let main = d.join("config");
        let text = "include other\n";
        let f = scan(text, Some(&main), &env());
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].file.ends_with("50-esc.conf"));
        assert_eq!(f[0].line, 2);

        // $VAR and ~ in the include path; a later unbind in the main file.
        let mut e = env();
        e.vars.insert("CFG".into(), d.display().to_string());
        let f = scan(
            "include $CFG/config.d/*\nunbindsym Escape\n",
            Some(&main),
            &e,
        );
        assert!(f.is_empty(), "{f:?}");

        // Include cycles terminate.
        std::fs::write(d.join("loop"), "include loop\nbindsym Escape exec x\n").unwrap();
        assert_eq!(scan("include loop\n", Some(&main), &env()).len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn helpers() {
        assert_eq!(
            split_args(r#"bindsym "a b" 'c d' "x\"y" z"#),
            vec!["bindsym", "a b", "c d", "x\"y", "z"]
        );
        assert!(glob_match(b"*.conf", b"x.conf"));
        assert!(!glob_match(b"*.conf", b"x.cfg"));
        assert!(glob_match(b"?0-*", b"10-a"));
        let e = env();
        assert_eq!(expand_env("~/x", &e), "/home/u/x");
        assert_eq!(expand_env("${HOME}/y/$HOME", &e), "/home/u/y//home/u");
        assert_eq!(expand_env("~user/x", &e), "~user/x");
    }
}
