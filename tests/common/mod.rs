//! Helpers for the `abort-watch` integration tests: spawn the real binary
//! with a scrubbed environment and read its JSON lines.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_wlroots-bridge");

/// A short private directory under /tmp (Wayland socket paths must stay
/// under 108 bytes), removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = PathBuf::from(format!(
            "/tmp/wb{}{}{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(p)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A running `wlroots-bridge abort-watch`.
pub struct Watcher {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    pub stderr: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Watcher {
    pub fn spawn(env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(BIN);
        cmd.arg("abort-watch")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn abort-watch");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("stdout line is not JSON: {line}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        let err = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let e2 = err.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                e2.lock().unwrap().push_str(&line);
                e2.lock().unwrap().push('\n');
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            stderr: err,
        }
    }

    /// The next stdout line within `limit`.
    pub fn next(&self, limit: Duration) -> Option<Value> {
        self.lines.recv_timeout(limit).ok()
    }

    pub fn expect(&self, limit: Duration) -> Value {
        self.next(limit).unwrap_or_else(|| {
            panic!(
                "no stdout line within {limit:?}; stderr:\n{}",
                self.stderr.lock().unwrap()
            )
        })
    }

    pub fn expect_ready(&self) -> Value {
        let v = self.expect(Duration::from_secs(10));
        assert_eq!(v["event"], "ready", "{v}");
        v
    }

    /// No stdout line for `d`.
    pub fn quiet_for(&self, d: Duration) -> Option<Value> {
        self.next(d)
    }

    pub fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().expect("stdin open");
        s.write_all(line.as_bytes()).unwrap();
        s.write_all(b"\n").unwrap();
        s.flush().unwrap();
    }

    /// Close stdin (EOF = clean exit).
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    pub fn signal(&self, sig: &str) {
        let ok = Command::new("kill")
            .args([&format!("-{sig}"), &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    pub fn wait(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < deadline, "watcher did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn stderr_text(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll `f` until it returns true or `limit` passes.
pub fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}
