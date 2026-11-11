# wlroots-bridge

First-party wlroots-Wayland bridge for Claude Desktop's Computer Use on Linux. A
single pure-Rust, statically-linked binary that replaces the shell-out chain
(`ydotool` for input, `grim` for screenshots, `hyprctl` / `swaymsg`+`jq` /
`niri msg` for window enumeration) on **wlroots-based Wayland sessions**
(Sway, Hyprland, Niri). It is a sibling of
[`x11-bridge`](https://github.com/patrickjaja/x11-bridge) (X11 / XWayland) and
[`kwin-portal-bridge`](https://github.com/patrickjaja/kwin-portal-bridge)
(KDE/Wayland) and speaks the **same kebab-case one-shot CLI + JSON contract**,
so the Claude Desktop JS executor (`js/cu_linux_executor.js` in
`claude-desktop-extra`) treats them interchangeably and picks a backend per
session without changing how it parses output.

**Status: experimental, v0.1.** All subcommands are implemented: output
enumeration (`wl_output` + `xdg-output`), `wlr-screencopy` capture (JPEG),
virtual-pointer / virtual-keyboard input synthesis, and foreign-toplevel window
queries + activation. Unit tests cover the pure logic (key-spec parsing, XKB
keymap generation, pixel decoding, JSON shapes); the live protocol paths are
exercised by a headless-sway smoke test in CI. The resolver is wired into
`claude-desktop-extra`'s `js/cu_linux_executor.js`, which bundles this binary and
drives it on Sway, Hyprland and Niri sessions.

## Sibling bridges

Four sibling projects cover the Linux session types with one shared JSON
contract:

| Bridge | Covers | Input / capture mechanism |
|---|---|---|
| `x11-bridge` | X11 and XWayland fallback | XTEST input + `GetImage` capture (stateless, one-shot) |
| `kwin-portal-bridge` | KDE Plasma / Wayland | RemoteDesktop + ScreenCast portal, PipeWire (long-lived session) |
| **`wlroots-bridge`** | **Sway / Hyprland / Niri (wlroots-Wayland)** | **virtual-pointer / virtual-keyboard + wlr-screencopy (stateless, one-shot)** |
| `gnome-portal-bridge` *(in development)* | GNOME / Wayland | portal + PipeWire |

## Requirements

The compositor must advertise the wlroots protocol extensions this bridge binds:

- `zwlr_virtual_pointer_manager_v1` (pointer input)
- `zwp_virtual_keyboard_manager_v1` (keyboard input)
- `zwlr_screencopy_manager_v1` (screenshots)
- `zwlr_foreign_toplevel_management_unstable_v1` (window list + activate),
  with `ext_foreign_toplevel_list_v1` as a list-only fallback for compositors
  that advertise only the ext protocol
- `zxdg_output_manager_v1` (logical output geometry)

Masked screenshots (`--keep-window`) on Hyprland capture each kept window on
its own: `ext_image_copy_capture_manager_v1` +
`ext_foreign_toplevel_image_capture_source_manager_v1` (Hyprland 0.54+) or
`hyprland_toplevel_export_manager_v1` v2. On sway they cut the composited
output. `doctor` reports which (`screenshot_mask_mode`,
`screenshot_mask_protocol`).

Sway, Hyprland, and **Niri all advertise the wlr protocols above** (including
the wlr foreign-toplevel manager with `activate` - verified against niri's
source), so window activation works on all three. Niri additionally gates these
as privileged protocols, disabled only for sandboxed / security-context clients.
Run `wlroots-bridge doctor` to see what the running compositor advertises (see
[CLI overview](#cli-overview)).

## Portability (static musl, incl. NixOS)

`wlroots-bridge` is **pure Rust with zero C dependencies** - it uses
`wayland-client`'s `client_rust` backend (no `libwayland`; the Hyprland
protocol XML it uses is vendored in `protocols/` and compiled in), generates its XKB
keymap text itself (no `libxkbcommon`), and uses the `image` crate for JPEG. It
builds as a **fully static musl binary** linked with `rust-lld`, so there is no
external cross-linker and no runtime library dependency at all. The same binary
runs on Arch, Ubuntu, Debian, Fedora, RHEL, and **NixOS** without patching an
interpreter or shipping shared libraries.

## Build

```sh
# Native release build:
cargo build --release

# Fully-static musl builds (recommended - no C linking anywhere):
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

`.cargo/config.toml` sets `rust-lld` + `+crt-static` for both musl targets, so
those commands work with just the toolchain installed - no `musl-gcc`, no
`gcc-aarch64-linux-gnu`.

### Reproducible local pipeline build

`scripts/pipeline-build-local.sh` builds **both** musl targets in a single
Docker run (mirroring CI) and drops the binaries in `./dist/`:

```sh
./scripts/pipeline-build-local.sh
# dist/wlroots-bridge          (x86_64, statically linked)
# dist/wlroots-bridge-aarch64  (aarch64, statically linked)
```

### Local end-to-end test (headless sway)

`scripts/test-headless-sway.sh` starts a throwaway headless sway compositor on a
private `WAYLAND_DISPLAY` and runs the full subcommand sequence against it
(doctor / screens / screenshot / zoom / input / held-button pair). This is the
only way to exercise the live protocol paths without a real Wayland session
(useful on an X11 host). Requires `sway`:

```sh
cargo build --release
scripts/test-headless-sway.sh
```

## CLI overview

One-shot subcommands. Each prints a single JSON object/array on stdout; on
failure it prints a message to stderr and exits 1 (clap parse errors exit 2).

| Subcommand | Purpose |
|---|---|
| `doctor` | Contract v1 report (always exit 0) + Wayland env and advertised globals |
| `screens` | Enumerate outputs (wl_output + xdg-output) |
| `windows` | Enumerate windows (foreign-toplevel; geometry/output/visibility from sway or Hyprland IPC) |
| `screenshot [--display N] [--keep-window ID]... [--mask-all]` | Capture a full monitor; mask mode shows only the kept windows on #1E1E1E (sway/Hyprland) |
| `zoom --display N --x --y --w --h [--keep-window ID]... [--mask-all]` | Capture a region within a monitor (same mask mode) |
| `pointer-move` / `-click` / `-scroll` / `-drag` | Pointer actions (virtual pointer) |
| `left-mouse-down` / `left-mouse-up` | Press/release-and-hold the left button |
| `key-sequence --keys <spec>` | Send a key combo, e.g. `ctrl+shift+tab` |
| `type --text <t>` | Type text as key events |
| `hold-key --key <k>... --duration-ms N` | Hold keys for a fixed duration |
| `frontmost-app` | Report the activated app |
| `app-under-point --x --y` | Unsupported on Wayland (returns null) |
| `cursor-position` | Unsupported on Wayland (exits 1) |
| `activate-window --window <id>` | Raise + focus a window (wlr foreign-toplevel; works on Sway/Hyprland/Niri) |
| `session-start` / `session-end` / `forget-session` | No-ops (`session-start` -> `{"ok":true,"session":"noop"}`; the others report `ended:false, daemon:"none"`) |
| `abort-watch` | Long-running Escape watcher (optional contract addition, sway and Hyprland): JSON lines on stdout, `suspend` / `resume` on stdin |

```sh
wlroots-bridge doctor
wlroots-bridge screens
wlroots-bridge screenshot --display eDP-1
wlroots-bridge zoom --display eDP-1 --x 100 --y 100 --w 400 --h 300
wlroots-bridge pointer-click --x 640 --y 480 --button left --count 2
wlroots-bridge key-sequence --keys 'ctrl+shift+tab' --repeat 1
wlroots-bridge type --text 'hello' --delay-ms 12
```

See [DESIGN.md](DESIGN.md) for the full subcommand -> JSON output contract, the
key-spec grammar, the coordinate system, the held-button mechanism, the keymap
generation approach, and the wlroots-specific contract deviations.

## CLI contract v1

The CLI follows the Computer Use bridge CLI contract v1 of claude-desktop-extra:
[docs/computer-use-bridge-contract.md](https://github.com/patrickjaja/claude-desktop-extra/blob/master/docs/computer-use-bridge-contract.md).
`doctor` always exits 0 and reports the required keys (`contract`, `bridge`,
`version`, `backend`, `session_match`, `backend_reachable`, `backend_error`,
`session_supported`, ...), with the compositor details as extras when reachable.
`session-end` and `forget-session` report `ended:false, daemon:"none"` (this
bridge has no session daemon or grant), and every coordinate flag accepts
negative values.

## Escape abort (`abort-watch`, optional contract addition)

`abort-watch` tells Claude Desktop when the user presses Escape on a physical
keyboard, so a running Computer Use turn stops. It is the one long-running
subcommand; the wire protocol (`ready` / `abort` / `ack` / `error` lines,
`suspend` / `resume`, exit codes, parent-death cleanup) comes from the shared
[cu-abort-watch](https://github.com/patrickjaja/cu-abort-watch) crate.

| Compositor | Mechanism | Escape still reaches the app? | `syntheticSafe` |
|---|---|---|---|
| sway (and sway-IPC forks) | `bindsym --input-device=<keyboard> --no-repeat --inhibited Escape nop claude-cu-abort` per physical keyboard, `binding` event | **No**: sway has no non-consuming `bindsym`, so while armed Escape is taken from applications | true |
| Hyprland, Lua config (0.55+) | `hl.bind("EsCaPe", hl.dsp.event(..), {non_consuming, dont_inhibit, device = {..}})` over `eval`, `custom>>claude-cu-abort` on socket2 | Yes (non-consuming) | true |
| Hyprland, hyprlang 0.55/0.56 | `keyword bindnpkd` (non-consuming, per-device) | Yes | true |
| Hyprland, hyprlang up to 0.54 | `keyword bindnpd` (non-consuming, global; `suspend` unbinds it) | Yes | false |
| niri, river, labwc, ... | none: `error` event "no runtime binding API on <compositor>", exit 2 | - | - |

The bridge's own virtual keyboard is never bound, so an Escape the model types
does not abort. On Hyprland with a Lua config, arming and disarming go through
`hyprctl eval`, which clears the config-error list Hyprland shows (unavoidable:
it is the only runtime bind API there; the errors return on the next config
reload). A bind someone removes at run time is noticed within 2 s and bound
again. When the user already binds a bare Escape (sway: in the
`default` mode of the config; Hyprland: outside submaps), `abort-watch` refuses
to arm and leaves that binding alone. Any compositor reply that is not exactly
the documented success undoes what was done and exits 2 (unverified means not
armed).

Binds are shared by every Claude profile of ONE compositor instance under
`$XDG_RUNTIME_DIR/claude-cu-esc/<sway|hyprland>/<instance>/`: the first watcher
binds, the last one unbinds, and binds left by a killed watcher are cleared by
the next one. To clear them without arming, run `abort-watch` with stdin
already at EOF (`wlroots-bridge abort-watch </dev/null`: arms, cleans up,
exits 0).

`doctor` reports `abortWatch: {mechanism, available, reason, stale,
skippedKeyboards, consumes}`: `stale` = a crashed watcher left binds,
`skippedKeyboards` = physical keyboards that cannot be bound right now,
`consumes` = whether arming takes Escape from applications (`true` on sway,
`false` on Hyprland 0.55+, `null` when unknown).
Details and limits: [DESIGN.md](DESIGN.md#abort-watch-contract-v2).

Tests: `tests/abort_mock.rs` (mock sway and Hyprland sockets, no compositor)
and `tests/abort_sway.rs` (real nested headless sway; skipped without `sway`,
fails instead with `WLROOTS_BRIDGE_REQUIRE_SWAY=1`, as in CI).

## Bundling into claude-desktop-extra

`wlroots-bridge` is intended to be consumed by
[`claude-desktop-extra`](https://github.com/patrickjaja/claude-desktop-extra) the
same way the other bridges are: the pre-built static binary is shipped inside the
package and the JS executor resolves it at runtime, in order:

1. the `WLROOTS_BRIDGE_BIN` environment variable (explicit override),
2. `process.resourcesPath` (the bundled copy inside the packaged app),
3. `wlroots-bridge` on `$PATH` (a system install).

Because the binary is fully static, the bundled copy runs on every supported
distro without extra runtime dependencies. The resolver lives in
`claude-desktop-extra`'s `js/cu_mode_preamble.js`, which publishes the resolved
path for `js/cu_linux_executor.js` to call.

## License

MIT. See [LICENSE](LICENSE). The vendored Hyprland protocol files in
`protocols/` are BSD-3-Clause (hyprland-protocols); see
[THIRD-PARTY.md](THIRD-PARTY.md).
