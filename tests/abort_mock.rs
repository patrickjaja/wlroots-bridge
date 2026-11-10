//! `abort-watch` against mock compositor sockets (no compositor needed):
//! the exact requests sent to Hyprland (Lua and hyprlang dialects) and sway,
//! the protocol lines, suspend/resume, reload, hot-plug and cleanup.

mod common;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TempDir, Watcher, wait_for};
use serde_json::{Value, json};

const T: Duration = Duration::from_secs(5);

// ------------------------------------------------------------ Hyprland

/// Models one Hyprland version: `lua` = 0.55+ Lua config manager (0.57),
/// `k_flag` (without `lua`) = 0.55/0.56 hyprlang, neither = 0.54 or older.
#[derive(Default)]
struct HyprState {
    lua: bool,
    k_flag: bool,
    user_escape: bool,
    /// A user bind with `ignore_mods` on Escape (only the 0.57 text `binds`
    /// output shows that flag).
    user_ignore_mods: bool,
    ours: bool,
    devices: Vec<String>,
    /// `j/status` answers something undocumented.
    status_garbage: bool,
    /// Bind requests answer `ok` but register nothing.
    lie_ok: bool,
    /// A user Escape bind made in Lua (`__lua <ref>`, described).
    user_escape_lua: bool,
    /// Our Lua bind objects are expired (something removed the binds).
    expired: bool,
    /// Hyprland 0.55/0.56 would have crashed: `remove()` ran on an expired
    /// object without the `tostring` guard.
    crashed: bool,
}

fn respond(st: &mut HyprState, req: &str) -> String {
    let marker = "claude-cu-abort";
    if req == "j/status" && st.status_garbage {
        return "permission denied".into();
    }
    if let Some(code) = req.strip_prefix("eval ") {
        if !st.lua {
            return if st.k_flag {
                "eval is only supported with the lua config manager".into()
            } else {
                "unknown request".into()
            };
        }
        if code.contains("hl.bind(") {
            st.ours = !st.lie_ok;
        }
        if code.contains(":remove()") {
            let guarded = code.contains("if tostring(b) ~= \"HL.Keybind(expired)\" then pcall(function() b:remove() end) end");
            if st.expired && !guarded {
                st.crashed = true;
            }
            st.ours = false;
            st.expired = false;
        }
        return "ok".into();
    }
    if let Some(rest) = req.strip_prefix("keyword bind") {
        let flags: String = rest.chars().take_while(|c| *c != ' ').collect();
        if st.lua {
            return "unknown request".into();
        }
        if flags.contains('k') && !st.k_flag {
            return "bind: invalid flag".into();
        }
        st.ours = !st.lie_ok;
        return "ok".into();
    }
    match req {
        "j/status" if st.lua => json!({"configProvider":"lua","backend":"drm"}).to_string(),
        "j/status" if st.k_flag => json!({"configProvider":"hyprlang","backend":"drm"}).to_string(),
        "j/binds" => {
            let mut v = vec![
                json!({"modmask":64,"submap":"","submap_universal":"false","key":"ESCAPE","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"exec","arg":"menu"}),
                json!({"modmask":0,"submap":"resize","submap_universal":"false","key":"escape","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"submap","arg":"reset"}),
            ];
            if st.user_escape {
                v.push(json!({"modmask":0,"submap":"","submap_universal":"false","key":"escape","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"exec","arg":"user"}));
            }
            if st.user_escape_lua {
                v.push(json!({"modmask":0,"submap":"","submap_universal":"false","key":"Escape","keycode":0,"catch_all":false,"mouse":false,"description":"Close notifications","dispatcher":"__lua","arg":"17"}));
            }
            if st.user_ignore_mods {
                v.push(json!({"modmask":64,"submap":"","submap_universal":"false","key":"Escape","keycode":0,"catch_all":false,"mouse":false,"description":"","dispatcher":"exec","arg":"ign"}));
            }
            if st.ours {
                v.push(json!({"modmask":0,"submap":"","submap_universal":"false","key":"EsCaPe","keycode":0,"catch_all":false,"mouse":false,"description":marker,"dispatcher":"event","arg":marker}));
            }
            Value::Array(v).to_string()
        }
        "binds" if st.lua => {
            let mut t = String::from(
                "bind\n\tflags: \n\tmodmask: 64\n\tsubmap: \n\tkey: ESCAPE\n\tkeycode: 0\n\tcatchall: false\n\tdescription: \n\tdispatcher: exec\n\targ: menu\n\n",
            );
            if st.user_ignore_mods {
                t.push_str("bind\n\tflags: ignore_mods\n\tmodmask: 64\n\tsubmap: \n\tkey: Escape\n\tkeycode: 0\n\tcatchall: false\n\tdescription: \n\tdispatcher: exec\n\targ: ign\n\n");
            }
            t
        }
        "j/devices" => {
            let kb: Vec<Value> = st
                .devices
                .iter()
                .map(|n| json!({"address":"0x1","name":n,"main":false}))
                .collect();
            json!({"mice":[],"keyboards":kb}).to_string()
        }
        "keyword unbind , EsCaPe" if !st.lua => {
            st.ours = false;
            "ok".into()
        }
        _ => "unknown request".into(),
    }
}

/// `claude-cu-esc/<mech>/<instance>` (the one entry that is not a
/// `-suspended` sibling).
fn instance_dir(rt: &std::path::Path, mech: &str) -> std::path::PathBuf {
    let base = rt.join("claude-cu-esc").join(mech);
    let mut found: Vec<_> = std::fs::read_dir(&base)
        .unwrap_or_else(|e| panic!("{}: {e}", base.display()))
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.path())
        .filter(|p| !p.to_string_lossy().ends_with("-suspended"))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one instance dir in {}: {found:?}",
        base.display()
    );
    found.remove(0)
}

struct MockHypr {
    rt: std::sync::Arc<TempDir>,
    sig: String,
    log: Arc<Mutex<Vec<String>>>,
    state: Arc<Mutex<HyprState>>,
    events: Arc<Mutex<Vec<UnixStream>>>,
}

impl MockHypr {
    fn start(st: HyprState) -> Self {
        Self::start_in(std::sync::Arc::new(TempDir::new("h")), "mocksig", st)
    }

    /// Another instance (signature `sig`) in the same runtime dir.
    fn start_in(rt: std::sync::Arc<TempDir>, sig: &str, st: HyprState) -> Self {
        let sig = sig.to_owned();
        let dir = rt.path().join("hypr").join(&sig);
        std::fs::create_dir_all(&dir).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(st));
        let events = Arc::new(Mutex::new(Vec::new()));

        let l1 = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let (lg, s) = (log.clone(), state.clone());
        std::thread::spawn(move || {
            for c in l1.incoming() {
                let Ok(mut c) = c else { break };
                let mut buf = vec![0u8; 65536];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let reply = respond(&mut s.lock().unwrap(), &req);
                lg.lock().unwrap().push(req);
                let _ = c.write_all(reply.as_bytes());
            }
        });
        let l2 = UnixListener::bind(dir.join(".socket2.sock")).unwrap();
        let ev = events.clone();
        std::thread::spawn(move || {
            for c in l2.incoming() {
                let Ok(c) = c else { break };
                ev.lock().unwrap().push(c);
            }
        });
        Self {
            rt,
            sig,
            log,
            state,
            events,
        }
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("XDG_RUNTIME_DIR", self.rt.path().display().to_string()),
            ("HYPRLAND_INSTANCE_SIGNATURE", self.sig.clone()),
        ]
    }

    fn spawn(&self) -> Watcher {
        let env = self.env();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        Watcher::spawn(&env)
    }

    fn push(&self, line: &str) {
        assert!(wait_for(T, || !self.events.lock().unwrap().is_empty()));
        for c in self.events.lock().unwrap().iter_mut() {
            let _ = c.write_all(format!("{line}\n").as_bytes());
        }
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn holder_dir(&self) -> std::path::PathBuf {
        let d = self
            .rt
            .path()
            .join("claude-cu-esc/hyprland")
            .join(&self.sig);
        assert!(d.is_dir(), "{}", d.display());
        d
    }

    fn spawn_closed_stdin(&self) -> Watcher {
        let mut w = self.spawn();
        w.close_stdin();
        w
    }

    fn doctor(&self) -> Value {
        let mut cmd = std::process::Command::new(common::BIN);
        cmd.arg("doctor").env_clear();
        for (k, v) in self.env() {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["abortWatch"].clone()
    }
}

fn devices(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn hypr_lua_binds_physical_keyboards_only_and_cleans_up() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&[
            "at-translated-set-2-keyboard",
            "hl-virtual-keyboard",
            "hl-virtual-keyboard-wlroots-bridge",
        ]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let r = w.expect_ready();
    assert_eq!(
        r,
        json!({"event":"ready","mechanism":"hyprland","syntheticSafe":true})
    );
    let bind = m
        .requests()
        .into_iter()
        .find(|r| r.contains("hl.bind("))
        .expect("a Lua bind was sent");
    assert!(
        bind.contains("list = { \"at-translated-set-2-keyboard\" }"),
        "{bind}"
    );
    // C1: the odd key spelling (0.55/0.56 `remove()` erases every bind with
    // the same key string); ruling: non-consuming; O4: fires while an app
    // inhibits shortcuts.
    assert!(bind.contains("hl.bind(\"EsCaPe\", "), "{bind}");
    assert!(bind.contains("non_consuming = true"), "{bind}");
    assert!(bind.contains("dont_inhibit = true"), "{bind}");
    assert!(!bind.contains("hl-virtual-keyboard"), "{bind}");
    assert!(m.holder_dir().join("bound.json").exists());
    // One holder set per instance: claude-cu-esc/hyprland/<signature>.
    assert_eq!(instance_dir(m.rt.path(), "hyprland"), m.holder_dir());

    // Unrelated socket2 traffic is ignored, ours aborts.
    m.push("activewindow>>kitty,~");
    m.push("custom>>something-else");
    m.push("custom>>claude-cu-abort");
    assert_eq!(w.expect(T), json!({"event":"abort"}));
    // A duplicate within the debounce window is one press.
    m.push("custom>>claude-cu-abort");
    assert_eq!(w.quiet_for(Duration::from_millis(300)), None);

    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    let reqs = m.requests();
    assert!(reqs.iter().any(|r| r.contains(":remove()")), "{reqs:?}");
    assert!(!m.state.lock().unwrap().ours);
    assert!(!m.holder_dir().join("bound.json").exists());
}

#[test]
fn hypr_legacy_device_flag() {
    let m = MockHypr::start(HyprState {
        k_flag: true,
        devices: devices(&["kbd-a", "kbd-b", "hl-virtual-keyboard-1"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    assert_eq!(w.expect_ready()["syntheticSafe"], true);
    let reqs = m.requests();
    assert!(
        reqs.contains(
            &"keyword bindnpkd , EsCaPe, kbd-a kbd-b, claude-cu-abort, event, claude-cu-abort"
                .to_owned()
        ),
        "{reqs:?}"
    );
    // syntheticSafe: suspend/resume are acked without touching the bind.
    w.send("suspend");
    assert_eq!(w.expect(T), json!({"event":"ack","cmd":"suspend"}));
    w.send("resume");
    assert_eq!(w.expect(T), json!({"event":"ack","cmd":"resume"}));
    assert!(m.state.lock().unwrap().ours);
    w.signal("TERM");
    assert_eq!(w.wait(T).code(), Some(0));
    assert_eq!(m.requests().last().unwrap(), "keyword unbind , EsCaPe");
}

#[test]
fn hypr_legacy_without_device_flag_falls_back_to_global_with_suspend() {
    let m = MockHypr::start(HyprState {
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    assert_eq!(
        w.expect_ready(),
        json!({"event":"ready","mechanism":"hyprland","syntheticSafe":false})
    );
    assert!(
        m.requests().contains(
            &"keyword bindnpd , EsCaPe, claude-cu-abort, event, claude-cu-abort".to_owned()
        )
    );
    w.send("suspend");
    assert_eq!(w.expect(T), json!({"event":"ack","cmd":"suspend"}));
    assert!(!m.state.lock().unwrap().ours, "suspend unbinds");
    // While suspended an event is muted.
    m.push("custom>>claude-cu-abort");
    assert_eq!(w.quiet_for(Duration::from_millis(300)), None);
    w.send("resume");
    assert_eq!(w.expect(T), json!({"event":"ack","cmd":"resume"}));
    assert!(m.state.lock().unwrap().ours, "resume binds again");
    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    assert!(!m.state.lock().unwrap().ours);
}

#[test]
fn hypr_refuses_when_user_binds_escape() {
    let m = MockHypr::start(HyprState {
        lua: true,
        user_escape: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error");
    let msg = e["message"].as_str().unwrap();
    assert!(
        msg.contains("already bound") && msg.contains("exec user"),
        "{msg}"
    );
    assert_eq!(w.wait(T).code(), Some(2));
    assert!(!m.requests().iter().any(|r| r.contains("hl.bind(")));
    // Nothing left behind: no holder file, no record.
    let left: Vec<_> = std::fs::read_dir(m.holder_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".lock")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn hypr_reload_rebinds_and_hotplug_resyncs() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    w.expect_ready();
    let binds = || {
        m.requests()
            .iter()
            .filter(|r| r.contains("hl.bind("))
            .count()
    };
    assert_eq!(binds(), 1);
    // A reload drops our bind: re-bound.
    m.state.lock().unwrap().ours = false;
    m.push("configreloaded>>");
    assert!(wait_for(T, || binds() == 2));
    // A reload that kept the bind: nothing to do.
    m.push("configreloaded>>");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(binds(), 2);
    // A new keyboard: picked up by the device poll.
    m.state.lock().unwrap().devices.push("kbd-b".into());
    assert!(wait_for(Duration::from_secs(6), || binds() == 3));
    let last = m
        .requests()
        .into_iter()
        .rfind(|r| r.contains("hl.bind("))
        .unwrap();
    assert!(last.contains("\"kbd-a\", \"kbd-b\""), "{last}");
    // A reload that adds a user Escape bind stops the watcher (exit 1).
    {
        let mut st = m.state.lock().unwrap();
        st.ours = false;
        st.user_escape = true;
    }
    m.push("configreloaded>>");
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(w.wait(T).code(), Some(1));
}

#[test]
fn hypr_doctor_reports_abort_watch() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let doctor = |m: &MockHypr| -> Value {
        let mut cmd = std::process::Command::new(common::BIN);
        cmd.arg("doctor").env_clear();
        for (k, v) in m.env() {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["abortWatch"].clone()
    };
    assert_eq!(
        doctor(&m),
        json!({"mechanism":"hyprland","available":true,"reason":null,
               "stale":false,"skippedKeyboards":[],"consumes":false})
    );
    m.state.lock().unwrap().user_escape = true;
    let d = doctor(&m);
    assert_eq!(d["available"], false);
    assert!(
        d["reason"].as_str().unwrap().contains("already bound"),
        "{d}"
    );
    // The doctor never binds anything.
    // O7: read-only requests only (an `eval` clears the user's config
    // error list).
    let reqs = m.requests();
    assert!(
        reqs.iter().all(|r| r.starts_with("j/") || r == "binds"),
        "{reqs:?}"
    );
}

// ---------------------------------------------------------------- sway

#[derive(Default)]
struct SwayState {
    reject_no_repeat: bool,
    config: String,
    inputs: Value,
    /// GET_BINDING_STATE name ("" = default).
    mode: String,
}

struct MockSway {
    rt: TempDir,
    commands: Arc<Mutex<Vec<String>>>,
    subs: Arc<Mutex<Vec<UnixStream>>>,
}

fn frame(ty: u32, body: &str) -> Vec<u8> {
    let mut m = b"i3-ipc".to_vec();
    m.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    m.extend_from_slice(&ty.to_ne_bytes());
    m.extend_from_slice(body.as_bytes());
    m
}

impl MockSway {
    fn start(st: SwayState) -> Self {
        let rt = TempDir::new("s");
        let sock = rt.path().join("sway.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(st));
        let subs = Arc::new(Mutex::new(Vec::new()));
        let (cm, s, sb) = (commands.clone(), state.clone(), subs.clone());
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { break };
                let (cm, s, sb) = (cm.clone(), s.clone(), sb.clone());
                std::thread::spawn(move || {
                    loop {
                        let mut head = [0u8; 14];
                        if c.read_exact(&mut head).is_err() {
                            return;
                        }
                        let len = u32::from_ne_bytes(head[6..10].try_into().unwrap()) as usize;
                        let ty = u32::from_ne_bytes(head[10..14].try_into().unwrap());
                        let mut p = vec![0u8; len];
                        c.read_exact(&mut p).unwrap();
                        let p = String::from_utf8(p).unwrap();
                        let reply = match ty {
                            0 => {
                                let st = s.lock().unwrap();
                                let res: Vec<Value> = p
                                    .split("; ")
                                    .map(|one| {
                                        cm.lock().unwrap().push(one.to_owned());
                                        if st.reject_no_repeat && one.contains("--no-repeat") {
                                            json!({"success":false,"parse_error":true,"error":"Unknown key or button '--no-repeat'"})
                                        } else {
                                            json!({"success":true})
                                        }
                                    })
                                    .collect();
                                Value::Array(res).to_string()
                            }
                            2 => {
                                c.write_all(&frame(2, "{\"success\":true}")).unwrap();
                                sb.lock().unwrap().push(c.try_clone().unwrap());
                                continue;
                            }
                            9 => json!({"config": s.lock().unwrap().config}).to_string(),
                            12 => {
                                let m = s.lock().unwrap().mode.clone();
                                json!({"name": if m.is_empty() { "default".to_owned() } else { m }})
                                    .to_string()
                            }
                            100 => s.lock().unwrap().inputs.to_string(),
                            _ => "{}".into(),
                        };
                        c.write_all(&frame(ty, &reply)).unwrap();
                    }
                });
            }
        });
        Self { rt, commands, subs }
    }

    fn spawn(&self) -> Watcher {
        let rt = self.rt.path().display().to_string();
        let sock = self.rt.path().join("sway.sock").display().to_string();
        Watcher::spawn(&[("XDG_RUNTIME_DIR", &rt), ("SWAYSOCK", &sock)])
    }

    fn event(&self, ty: u32, body: Value) {
        assert!(wait_for(T, || !self.subs.lock().unwrap().is_empty()));
        for c in self.subs.lock().unwrap().iter_mut() {
            let _ = c.write_all(&frame(ty, &body.to_string()));
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn sway_inputs() -> Value {
    json!([
        {"identifier":"1:1:AT_Translated_Set_2_keyboard","name":"AT Translated Set 2 keyboard","type":"keyboard","libinput":{}},
        {"identifier":"0:0:wlr_virtual_keyboard_v1","name":"wlr_virtual_keyboard_v1","type":"keyboard"},
        {"identifier":"2:7:SynPS/2_Synaptics_TouchPad","name":"SynPS/2 Synaptics TouchPad","type":"touchpad","libinput":{}}
    ])
}

const BINDING: u32 = 0x8000_0005;
const INPUT: u32 = 0x8000_0015;
const WORKSPACE: u32 = 0x8000_0000;

#[test]
fn sway_mock_binds_unbinds_and_falls_back_without_no_repeat() {
    let m = MockSway::start(SwayState {
        reject_no_repeat: true,
        config: "set $mod Mod4\nbindsym $mod+Escape exec menu\nmode \"resize\" {\n bindsym Escape mode default\n}\n".into(),
        inputs: sway_inputs(),
        ..Default::default()
    });
    let mut w = m.spawn();
    assert_eq!(
        w.expect_ready(),
        json!({"event":"ready","mechanism":"sway","syntheticSafe":true})
    );
    assert_eq!(
        m.commands(),
        vec![
            "mode default bindsym --input-device=1:1:AT_Translated_Set_2_keyboard --no-repeat --inhibited Escape nop claude-cu-abort",
            "mode default bindsym --input-device=1:1:AT_Translated_Set_2_keyboard --inhibited Escape nop claude-cu-abort",
        ]
    );
    m.event(
        BINDING,
        json!({"change":"run","binding":{"command":"nop claude-cu-abort","symbol":"Escape","input_type":"keyboard"}}),
    );
    assert_eq!(w.expect(T), json!({"event":"abort"}));
    // A user binding running is not ours.
    m.event(
        BINDING,
        json!({"change":"run","binding":{"command":"exec menu"}}),
    );
    assert_eq!(w.quiet_for(Duration::from_millis(300)), None);

    // Hot-plug: a physical keyboard is bound with the learned spelling, a
    // virtual one is not.
    m.event(INPUT, json!({"change":"added","input":{"identifier":"0:0:wlr_virtual_keyboard_v1","name":"wlr_virtual_keyboard_v1","type":"keyboard"}}));
    m.event(INPUT, json!({"change":"added","input":{"identifier":"3:3:USB_Keyboard","name":"USB Keyboard","type":"keyboard"}}));
    assert!(wait_for(T, || m.commands().len() == 3));
    assert_eq!(
        m.commands()[2],
        "mode default bindsym --input-device=3:3:USB_Keyboard --inhibited Escape nop claude-cu-abort"
    );

    // Reload: everything bound again.
    m.event(
        WORKSPACE,
        json!({"change":"reload","old":null,"current":null}),
    );
    assert!(wait_for(T, || m.commands().len() == 5));

    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    let c = m.commands();
    assert_eq!(
        &c[c.len() - 2..],
        &[
            "mode default unbindsym --input-device=1:1:AT_Translated_Set_2_keyboard --inhibited Escape",
            "mode default unbindsym --input-device=3:3:USB_Keyboard --inhibited Escape",
        ]
    );
}

#[test]
fn sway_mock_refuses_user_escape_binding() {
    let m = MockSway::start(SwayState {
        config: "set $quit Escape\nbindsym $quit exec user-thing\n".into(),
        inputs: sway_inputs(),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("bindsym Escape exec user-thing"), "{msg}");
    assert_eq!(w.wait(T).code(), Some(2));
    assert!(m.commands().is_empty(), "nothing bound or unbound");
}

#[test]
fn sway_mock_compositor_exit_fails_and_cleans_up() {
    let m = MockSway::start(SwayState {
        inputs: sway_inputs(),
        ..Default::default()
    });
    let mut w = m.spawn();
    w.expect_ready();
    m.event(0x8000_0006, json!({"change":"exit"}));
    let e = w.expect(T);
    assert_eq!(e["event"], "error");
    assert_eq!(w.wait(T).code(), Some(1));
    assert!(m.commands().last().unwrap().contains("unbindsym"));
}

// ---------------------------------------------------------- unsupported

#[test]
fn other_compositors_error_and_exit_2() {
    let rt = TempDir::new("o");
    let rts = rt.path().display().to_string();
    for (env, name) in [
        (vec![("NIRI_SOCKET", "/nonexistent")], "niri"),
        (vec![("XDG_CURRENT_DESKTOP", "river")], "river"),
        (vec![], "this compositor"),
    ] {
        let mut e: Vec<(&str, &str)> = env;
        e.push(("XDG_RUNTIME_DIR", &rts));
        let mut w = Watcher::spawn(&e);
        assert_eq!(
            w.expect(T),
            json!({"event":"error","message":format!("no runtime binding API on {name}")})
        );
        assert_eq!(w.wait(T).code(), Some(2));
    }
    // Nothing created in the runtime dir.
    assert!(std::fs::read_dir(rt.path()).unwrap().next().is_none());
}

// ------------------------------------------------------- review fixes

#[test]
fn hypr_dialect_probe_never_evals() {
    for st in [
        HyprState {
            lua: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            k_flag: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
    ] {
        let m = MockHypr::start(st);
        let mut w = m.spawn();
        w.expect_ready();
        w.close_stdin();
        assert_eq!(w.wait(T).code(), Some(0));
        let evals: Vec<String> = m
            .requests()
            .into_iter()
            .filter(|r| r.starts_with("eval"))
            .collect();
        // Only the bind and the unbind may run Lua.
        assert!(
            evals
                .iter()
                .all(|e| e.contains("hl.bind(") || e.contains(":remove()")),
            "{evals:?}"
        );
    }
}

#[test]
fn hypr_ignore_mods_user_bind_refuses() {
    let m = MockHypr::start(HyprState {
        lua: true,
        user_ignore_mods: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert!(e["message"].as_str().unwrap().contains("ign"), "{e}");
    assert_eq!(w.wait(T).code(), Some(2));
}

#[test]
fn hypr_instances_have_separate_holder_sets() {
    let rt = std::sync::Arc::new(TempDir::new("h"));
    let a = MockHypr::start_in(
        rt.clone(),
        "instA",
        HyprState {
            lua: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
    );
    let b = MockHypr::start_in(
        rt.clone(),
        "instB",
        HyprState {
            lua: true,
            devices: devices(&["kbd-b"]),
            ..Default::default()
        },
    );
    let mut wa = a.spawn();
    wa.expect_ready();
    let mut wb = b.spawn();
    wb.expect_ready();
    assert!(a.state.lock().unwrap().ours, "instance A bound");
    assert!(
        b.state.lock().unwrap().ours,
        "instance B bound too (its own first holder)"
    );
    wa.close_stdin();
    assert_eq!(wa.wait(T).code(), Some(0));
    assert!(!a.state.lock().unwrap().ours, "A's last holder unbound A");
    assert!(b.state.lock().unwrap().ours, "B untouched");
    wb.close_stdin();
    assert_eq!(wb.wait(T).code(), Some(0));
    assert!(!b.state.lock().unwrap().ours);
}

#[test]
fn hypr_heal_with_closed_stdin_clears_stale_bind() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut a = m.spawn();
    a.expect_ready();
    a.signal("KILL");
    a.wait(T);
    assert!(
        m.state.lock().unwrap().ours,
        "the crashed holder left its bind"
    );
    let d = m.doctor();
    assert_eq!(d["stale"], true, "{d}");

    let mut heal = m.spawn_closed_stdin();
    assert_eq!(heal.wait(T).code(), Some(0), "{}", heal.stderr_text());
    assert!(!m.state.lock().unwrap().ours, "stale bind removed");
    let dir = m.holder_dir();
    assert!(!dir.join("bound.json").exists());
    let holders: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.parse::<u32>().is_ok())
        .collect();
    assert!(holders.is_empty(), "{holders:?}");
    assert_eq!(m.doctor()["stale"], false);
}

#[test]
fn hypr_global_suspend_is_counted_across_holders() {
    let m = MockHypr::start(HyprState {
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut a = m.spawn();
    assert_eq!(a.expect_ready()["syntheticSafe"], false);
    let mut b = m.spawn();
    b.expect_ready();
    let bound = || m.state.lock().unwrap().ours;
    a.send("suspend");
    assert_eq!(a.expect(T)["cmd"], "suspend");
    b.send("suspend");
    assert_eq!(b.expect(T)["cmd"], "suspend");
    assert!(!bound());
    // A reload while suspended must not bind again.
    m.push("configreloaded>>");
    std::thread::sleep(Duration::from_millis(400));
    assert!(!bound(), "reload re-bound during a suspend");
    a.send("resume");
    assert_eq!(a.expect(T)["cmd"], "resume");
    assert!(!bound(), "A's resume re-bound while B is still suspended");
    b.send("resume");
    assert_eq!(b.expect(T)["cmd"], "resume");
    assert!(bound(), "last resume binds");
    a.close_stdin();
    b.close_stdin();
    a.wait(T);
    b.wait(T);
    assert!(!bound());
}

#[test]
fn hypr_device_poll_is_not_starved_by_socket2_traffic() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    w.expect_ready();
    let binds = || {
        m.requests()
            .iter()
            .filter(|r| r.contains("hl.bind("))
            .count()
    };
    assert_eq!(binds(), 1);
    m.state.lock().unwrap().devices.push("kbd-b".into());
    // Unrelated events every 200 ms: the socket2 read never times out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline && binds() < 2 {
        m.push("activewindow>>kitty,~");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(binds(), 2, "device change not picked up under traffic");
    w.close_stdin();
    w.wait(T);
}

#[test]
fn stale_swaysock_falls_back_to_hyprland() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut env = m.env();
    env.push(("SWAYSOCK", "/nonexistent/sway-ipc.sock".into()));
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut w = Watcher::spawn(&env);
    assert_eq!(w.expect_ready()["mechanism"], "hyprland");
    w.close_stdin();
    w.wait(T);
}

fn odd_inputs() -> Value {
    json!([
        {"identifier":"1:1:a;b,c","name":"a;b,c","type":"keyboard","libinput":{}},
        {"identifier":"0:0:wlr_virtual_keyboard_v1","name":"wlr_virtual_keyboard_v1","type":"keyboard"}
    ])
}

#[test]
fn sway_mock_quotes_unusual_identifiers() {
    let m = MockSway::start(SwayState {
        inputs: odd_inputs(),
        ..Default::default()
    });
    let mut w = m.spawn();
    w.expect_ready();
    // `mode default ...` keeps quotes literally, so a quoted identifier is
    // bound with the plain form while sway is in the default mode.
    assert_eq!(
        m.commands(),
        vec![
            r#"bindsym "--input-device=1:1:a;b,c" --no-repeat --inhibited Escape nop claude-cu-abort"#
        ]
    );
    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    assert_eq!(
        m.commands().last().unwrap(),
        r#"unbindsym "--input-device=1:1:a;b,c" --no-repeat --inhibited Escape"#
    );
}

#[test]
fn sway_mock_fails_when_no_physical_keyboard_can_be_bound() {
    let m = MockSway::start(SwayState {
        inputs: odd_inputs(),
        mode: "resize".into(),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert!(e["message"].as_str().unwrap().contains("keyboard"), "{e}");
    assert_eq!(w.wait(T).code(), Some(2));
    assert!(
        m.commands()
            .iter()
            .all(|c| !c.starts_with("bindsym") && !c.starts_with("mode default bindsym"))
    );
}

// ------------------------------------------- unverified means not armed

#[test]
fn hypr_undocumented_status_reply_does_not_arm() {
    let m = MockHypr::start(HyprState {
        lua: true,
        status_garbage: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(w.wait(T).code(), Some(2));
    let reqs = m.requests();
    assert!(
        reqs.iter()
            .all(|r| !r.starts_with("eval") && !r.starts_with("keyword")),
        "{reqs:?}"
    );
}

#[test]
fn hypr_ok_without_a_registered_bind_does_not_arm() {
    for st in [
        HyprState {
            lua: true,
            lie_ok: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            k_flag: true,
            lie_ok: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
    ] {
        let m = MockHypr::start(st);
        let mut w = m.spawn();
        let e = w.expect(T);
        assert_eq!(e["event"], "error", "{e}");
        assert!(
            e["message"].as_str().unwrap().contains("not in `binds`"),
            "{e}"
        );
        assert_eq!(w.wait(T).code(), Some(2));
        // Undone: an unbind followed the bind.
        let reqs = m.requests();
        assert!(
            reqs.iter()
                .any(|r| r.contains(":remove()") || r == "keyword unbind , EsCaPe"),
            "{reqs:?}"
        );
        assert!(!m.holder_dir().join("bound.json").exists());
    }
}

#[test]
fn hypr_doctor_consumes_by_dialect() {
    for (st, want) in [
        (
            HyprState {
                lua: true,
                ..Default::default()
            },
            json!(false),
        ),
        (
            HyprState {
                k_flag: true,
                ..Default::default()
            },
            json!(false),
        ),
        (HyprState::default(), Value::Null),
    ] {
        let m = MockHypr::start(st);
        assert_eq!(m.doctor()["consumes"], want);
    }
}

#[test]
fn sway_mock_unexpressible_identifier_is_reported_not_bound() {
    let m = MockSway::start(SwayState {
        inputs: json!([{"identifier":"1:1:q\"uote","name":"q\"uote","type":"keyboard","libinput":{}}]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert!(
        e["message"].as_str().unwrap().contains("cannot express"),
        "{e}"
    );
    assert_eq!(w.wait(T).code(), Some(2));
    assert!(m.commands().is_empty());
}

// ------------------------------------------------------ VM-run findings

/// Hyprland 0.55/0.56 SIGSEGV: something removed our binds at run time (a
/// user's `hl.unbind("escape")`), the stored objects are expired, and an
/// unguarded `b:remove()` dereferences null. Every Lua unbind must skip them.
#[test]
fn hypr_lua_unbind_never_touches_expired_objects() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    w.expect_ready();
    {
        let mut st = m.state.lock().unwrap();
        st.ours = false; // the user's `hl.unbind("escape")`
        st.expired = true;
    }
    w.close_stdin();
    assert_eq!(w.wait(T).code(), Some(0));
    assert!(
        !m.state.lock().unwrap().crashed,
        "unguarded remove() on an expired bind"
    );
    let unbind = m
        .requests()
        .into_iter()
        .rfind(|r| r.contains(":remove()"))
        .unwrap();
    assert_eq!(
        unbind,
        "eval for _, b in ipairs(__claude_cu_abort or {}) do if tostring(b) ~= \"HL.Keybind(expired)\" then pcall(function() b:remove() end) end end\n__claude_cu_abort = nil"
    );
}

/// A bind removed at run time (no event exists for it) is noticed by the
/// poll and bound again, in every variant; the Lua path clears the table of
/// expired objects first, without crashing.
#[test]
fn hypr_bind_removed_at_run_time_is_restored() {
    for st in [
        HyprState {
            lua: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            k_flag: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
    ] {
        let m = MockHypr::start(st);
        let mut w = m.spawn();
        w.expect_ready();
        let binds = || {
            m.requests()
                .iter()
                .filter(|r| r.contains("hl.bind(") || r.starts_with("keyword bind"))
                .count()
        };
        let before = binds();
        {
            let mut st = m.state.lock().unwrap();
            st.ours = false;
            st.expired = true;
        }
        assert!(
            wait_for(Duration::from_secs(6), || m.state.lock().unwrap().ours),
            "not re-bound"
        );
        assert!(binds() > before);
        assert!(!m.state.lock().unwrap().crashed);
        w.close_stdin();
        assert_eq!(w.wait(T).code(), Some(0));
        assert!(!m.state.lock().unwrap().ours);
    }
}

#[test]
fn hypr_conflict_message_names_lua_binds_by_description() {
    let m = MockHypr::start(HyprState {
        lua: true,
        user_escape_lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut w = m.spawn();
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("Escape -> \"Close notifications\""), "{msg}");
    assert!(!msg.contains("__lua"), "{msg}");
    assert_eq!(w.wait(T).code(), Some(2));
}

// ---------------------------------------- rollback after a failed arm

fn holder_entries(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n != ".lock")
                .collect()
        })
        .unwrap_or_default()
}

/// A failure AFTER the bind (debug-build hook standing in for a failed
/// thread spawn) must undo everything: bind, record, holder registration.
#[test]
fn hypr_failure_after_bind_leaves_nothing_behind() {
    for st in [
        HyprState {
            lua: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            k_flag: true,
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
        HyprState {
            devices: devices(&["kbd-a"]),
            ..Default::default()
        },
    ] {
        let m = MockHypr::start(st);
        let mut env = m.env();
        env.push(("WLROOTS_BRIDGE_TEST_FAIL_AFTER_BIND", "1".into()));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut w = Watcher::spawn(&env);
        let e = w.expect(T);
        assert_eq!(e["event"], "error", "{e}");
        assert!(
            e["message"].as_str().unwrap().contains("injected failure"),
            "{e}"
        );
        assert_eq!(w.wait(T).code(), Some(2));
        let reqs = m.requests();
        assert!(
            reqs.iter()
                .any(|r| r.contains("hl.bind(") || r.starts_with("keyword bind")),
            "the bind ran first: {reqs:?}"
        );
        assert!(!m.state.lock().unwrap().ours, "bind undone: {reqs:?}");
        assert!(
            holder_entries(&m.holder_dir()).is_empty(),
            "{:?}",
            holder_entries(&m.holder_dir())
        );
    }
}

#[test]
fn sway_failure_after_bind_leaves_nothing_behind() {
    let m = MockSway::start(SwayState {
        inputs: sway_inputs(),
        ..Default::default()
    });
    let rt = m.rt.path().display().to_string();
    let sock = m.rt.path().join("sway.sock").display().to_string();
    let mut w = Watcher::spawn(&[
        ("XDG_RUNTIME_DIR", &rt),
        ("SWAYSOCK", &sock),
        ("WLROOTS_BRIDGE_TEST_FAIL_AFTER_BIND", "1"),
    ]);
    let e = w.expect(T);
    assert_eq!(e["event"], "error", "{e}");
    assert_eq!(w.wait(T).code(), Some(2));
    assert_eq!(
        m.commands(),
        vec![
            "mode default bindsym --input-device=1:1:AT_Translated_Set_2_keyboard --no-repeat --inhibited Escape nop claude-cu-abort",
            "mode default unbindsym --input-device=1:1:AT_Translated_Set_2_keyboard --no-repeat --inhibited Escape",
        ]
    );
    let base = m.rt.path().join("claude-cu-esc/sway");
    for d in std::fs::read_dir(&base).unwrap().filter_map(|e| e.ok()) {
        assert!(
            holder_entries(&d.path()).is_empty(),
            "{:?}",
            holder_entries(&d.path())
        );
    }
}

/// Heal (`abort-watch </dev/null`) next to a LIVE watcher: joins as a second
/// holder, exits 0, leaves the bind and the live watcher alone.
#[test]
fn hypr_heal_next_to_a_live_holder_leaves_the_bind() {
    let m = MockHypr::start(HyprState {
        lua: true,
        devices: devices(&["kbd-a"]),
        ..Default::default()
    });
    let mut a = m.spawn();
    a.expect_ready();
    let before = m.requests().len();
    let mut heal = m.spawn_closed_stdin();
    assert_eq!(heal.wait(T).code(), Some(0), "{}", heal.stderr_text());
    assert!(m.state.lock().unwrap().ours, "bind untouched");
    let after: Vec<String> = m.requests()[before..].to_vec();
    assert!(
        after
            .iter()
            .all(|r| !r.contains("hl.bind(") && !r.contains(":remove()")),
        "heal must not bind or unbind: {after:?}"
    );
    assert!(m.holder_dir().join("bound.json").exists());
    m.push("custom>>claude-cu-abort");
    assert_eq!(a.expect(T), serde_json::json!({"event":"abort"}));
    a.close_stdin();
    assert_eq!(a.wait(T).code(), Some(0));
    assert!(!m.state.lock().unwrap().ours);
}
