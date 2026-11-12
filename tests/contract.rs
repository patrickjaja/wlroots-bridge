//! CLI contract v1 checks (docs/computer-use-bridge-contract.md in
//! claude-desktop-extra). Runs the real binary with a scrubbed environment and
//! no Wayland compositor, so it needs no sway and works in plain `cargo test`.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wlroots-bridge-contract-{}-{tag}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) -> Output {
    let home = scratch("home");
    Command::new(env!("CARGO_BIN_EXE_wlroots-bridge"))
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .expect("spawn wlroots-bridge")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON value")
}

#[test]
fn version_format() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.trim(),
        format!("wlroots-bridge {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn doctor_without_compositor_exits_zero_with_required_keys() {
    let out = run(&["doctor"]);
    assert_eq!(out.status.code(), Some(0));
    let v = stdout_json(&out);
    assert_eq!(v["contract"], 1);
    assert_eq!(v["bridge"], "wlroots-bridge");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["backend"], "wlroots");
    for key in [
        "session_match",
        "backend_reachable",
        "session_supported",
        "restore_token_present",
        "daemon_running",
    ] {
        assert_eq!(v[key], false, "{key} must be false without a compositor");
    }
    assert!(v["backend_error"].is_string());
    assert!(v["restore_token_path"].is_null());
    assert!(v["session_socket"].is_null());
    // Mask keys are present (additive) and say "no mask" here.
    assert_eq!(v["screenshot_mask"], false);
    assert_eq!(v["screenshot_mask_gaps"], serde_json::json!([]));
    assert!(v["screenshot_mask_mode"].is_null());
    assert!(v["screenshot_mask_protocol"].is_null());
    // Overlay capabilities (additive), next to the mask keys: all false here.
    assert_eq!(
        v["capabilities"],
        serde_json::json!({
            "overlay": false,
            "overlay_alpha_modifier": false,
            "overlay_fractional_scale": false
        })
    );
    // Contract v2 key: no sway / Hyprland in the scrubbed environment.
    assert_eq!(
        v["abortWatch"],
        serde_json::json!({"mechanism": null, "available": false,
            "reason": "no runtime binding API on this compositor",
            "stale": false, "skippedKeyboards": [], "consumes": null})
    );
}

#[test]
fn session_end_is_exact() {
    let out = run(&["session-end"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        r#"{"ok":true,"ended":false,"daemon":"none"}"#
    );
}

#[test]
fn session_start_is_noop() {
    let out = run(&["session-start"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout_json(&out),
        serde_json::json!({"ok": true, "session": "noop"})
    );
}

#[test]
fn forget_session_is_exact_and_has_no_side_effects() {
    let out = run(&["forget-session"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        r#"{"ok":true,"ended":false,"daemon":"none","token_removed":false,"token_path":null}"#
    );
    let home = scratch("home");
    assert!(
        !home.join("state").exists(),
        "forget-session must not create the state dir"
    );
    assert!(
        !home.join("run").exists(),
        "forget-session must not create the runtime dir"
    );
}

#[test]
fn help_exits_zero() {
    for sub in ["screens", "windows", "session-start"] {
        let out = run(&[sub, "--help"]);
        assert_eq!(out.status.code(), Some(0), "{sub} --help");
    }
}

#[test]
fn negative_coordinates_parse() {
    // `--help` after the flags short-circuits before any Wayland connection;
    // a parse error would exit 2.
    let cases: &[&[&str]] = &[
        &["pointer-move", "--x", "-5", "--y", "-5", "--help"],
        &["pointer-click", "--x", "-5", "--y", "-5", "--help"],
        &[
            "pointer-scroll",
            "--x",
            "-5",
            "--y",
            "-5",
            "--dx",
            "-1",
            "--dy",
            "-2",
            "--help",
        ],
        &[
            "pointer-drag",
            "--from-x",
            "-1",
            "--from-y",
            "-2",
            "--to-x",
            "-3",
            "--to-y",
            "-4",
            "--help",
        ],
        &[
            "zoom", "--x", "-100", "--y", "-50", "--w", "10", "--h", "10", "--help",
        ],
        &["app-under-point", "--x", "-5", "--y", "-5", "--help"],
    ];
    for args in cases {
        let out = run(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn negative_coordinates_reach_the_backend() {
    // Without --help the parse must succeed and the run fail at the Wayland
    // connection (exit 1), never as a usage error (exit 2).
    let out = run(&["pointer-move", "--x", "-5", "--y", "-5"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn bogus_subcommand_exits_two() {
    let out = run(&["definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(2));
}

fn run_with_stdin(args: &[&str], stdin: &[u8]) -> Output {
    use std::io::Write;
    use std::process::Stdio;
    let home = scratch("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_wlroots-bridge"))
        .args(args)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wlroots-bridge");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn ax_usage_errors_exit_two() {
    for args in [
        &["ax-enable"][..],
        &["ax-restore", "--holder-pid", "x"],
        &["ax-find"],
        &["ax-action", "--scope", "s", "--ref", "r"],
        &[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "bogus",
        ],
        &[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-text",
            "--text",
            "no",
        ],
    ] {
        assert_eq!(run(args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn doctor_has_atspi_key_and_window_ipc() {
    let out = run(&["doctor"]);
    assert_eq!(out.status.code(), Some(0));
    let v = stdout_json(&out);
    assert!(v["atspi"]["bus"].is_boolean());
    assert!(v["atspi"].get("window_ipc").is_some());
    assert!(v["atspi"]["window_ipc"].is_null());
}

#[test]
fn ax_action_set_text_rejects_invalid_utf8_stdin_with_exit_two() {
    let out = run_with_stdin(
        &[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-text",
        ],
        &[0xff, 0xfe],
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn ax_action_click_without_bus_is_unavailable_exit_zero() {
    let out = run_with_stdin(
        &[
            "ax-action",
            "--scope",
            "v1|:1.1|/x|1",
            "--ref",
            ":1.1/org/a11y/atspi/accessible/1",
            "--action",
            "click",
        ],
        b"",
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout_json(&out)["unavailable"].is_string());
}

#[test]
fn ax_features_prints_the_crates_list_without_a_session() {
    // No Wayland display, no D-Bus: must still succeed.
    let out = run(&["ax-features"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        stdout_json(&out),
        serde_json::json!({ "ax_features": cu_atspi::features() })
    );
    assert_eq!(cu_atspi::features().len(), 13);
}
