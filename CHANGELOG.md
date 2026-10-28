# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **CLI contract v1.** `doctor` always exits 0 and reports the contract keys
  (`contract`, `bridge`, `version`, `backend`, `session_match`,
  `backend_reachable`, `backend_error`, `session_supported`, ...) with the old
  keys as extras when a compositor answers; `session-end` reports
  `ended:false, daemon:"none"`; new `forget-session`; `tests/contract.rs`.

### Fixed

- **X keysym names for punctuation.** `key-sequence` and `hold-key` rejected
  the names xdotool takes (`ctrl+comma`, `ctrl+bracketleft`, `asciitilde`,
  `quotedbl`, ...) with "unsupported key name". Any name the built-in tables do
  not know is now looked up among the X keysym names, case-insensitively, with
  an exact-case match preferred (`Eacute` vs `eacute`).
- **Modifiers on wlroots compositors and on niri (measured on Sway).** wlroots,
  and niri releases on smithay 0.7, do not derive modifier state from a virtual
  keyboard's key events (confirmed in their source), so on such sessions
  `key-sequence ctrl+c`, `hold-key` and
  `pointer-click --modifier` delivered the bare key or click. Each modifier
  press or release now also sends `modifiers()` with the depressed mask
  (Shift `0x1`, Control `0x4`, Mod1 `0x8`, Mod4 `0x40`); the mask is back at 0
  when the command ends. SIGTERM, SIGINT or SIGHUP during `hold-key` or a
  `pointer-click --modifier` burst releases the keys and the mask before the
  bridge exits (`128 + signal`), so a stopped command does not leave Ctrl
  held.
- **First input lost on a seat without input devices.** Sway advertises a
  seat's pointer or keyboard capability only while a device provides it, so in
  a headless or nested session the bridge's fresh virtual device is what adds
  it, and clients had not bound `wl_pointer` / `wl_keyboard` yet when the
  events arrived: clicks did nothing and `type` dropped its first character.
  The bridge now reads the seat's capabilities before creating a device, does a
  roundtrip after creating it, and waits 50 ms only when the device added the
  capability. A seat that already has a keyboard or pointer, the usual desktop,
  pays no wait.
- **Screencopy wait is a 2 s wall-clock deadline, not 200 roundtrips.** Each
  roundtrip returns as soon as the compositor answers its sync request, before
  a frame is rendered, so the old wait ended too early for compositors that
  copy into the buffer on their next rendered frame (Hyprland at 4K, others
  under load) and failed with "screencopy did not complete (no ready event)". The bridge now polls the
  Wayland socket until `ready`, `failed` or 2 s; a compositor that answers at
  once still returns at once, only the upper bound grows. The timeout error
  reports the elapsed milliseconds.
- **Pointer on layouts that do not start at `(0,0)`.** Absolute motion now maps
  onto the layout's bounding box with its real origin (`x - min_x`,
  `y - min_y`, extents `max - min`, clamped to `extent - 1`), which is how
  wlroots' `wlr_cursor` interprets `motion_absolute`. Before, the extent was the
  right/bottom edge and negative coordinates clamped to 0, so a monitor left of
  or above the origin was unreachable and every point landed shifted by the
  negative offset.
- **Screenshots of rotated or flipped outputs.** Capture reads
  `wl_output.transform` and re-orients the screencopy buffer (after undoing
  y-invert) to the output's logical orientation for all eight transforms, so
  the image size and content match `screens`; `zoom` crops the right region.
  `screens` computes the fractional `scale` from the transform-swapped mode, so
  a rotated unscaled output reports 1.0 instead of e.g. 1.78.

## [0.1.0] - 2026-07-06

Initial experimental release. First-party wlroots-Wayland backend for Claude
Desktop's Computer Use, the sibling of `x11-bridge` and `kwin-portal-bridge`,
speaking the same one-shot CLI + JSON contract.

### Added

- **Output enumeration** (`screens`) via `wl_output` + `zxdg_output_manager_v1`:
  logical position/size, connector name, integer and fractional scale, refresh.
  The output at logical `(0,0)` is designated primary/active (Wayland has no
  primary concept).
- **Screenshot / zoom** (`screenshot`, `zoom`) via `zwlr_screencopy_manager_v1`
  (the `grim` protocol) into a `wl_shm` buffer. Handles xrgb/argb/xbgr/abgr8888
  and the y-invert flag. Downscale caps (long edge 1568, total 1.15 Mpx) and
  JPEG quality 75 match `x11-bridge` / `kwin-portal-bridge` exactly. `zoom`
  scales the logical crop region by the buffer/logical ratio for fractional
  displays.
- **Pointer input** (`pointer-move`/`-click`/`-scroll`/`-drag`) via
  `zwlr_virtual_pointer_manager_v1`. Absolute motion maps into the union of all
  outputs' logical extents (correct multi-monitor coordinates). Scroll is ~15
  units/notch, positive-down / positive-right (KDE / x11-bridge convention).
- **Held-button pair** (`left-mouse-down` / `left-mouse-up`) via a daemonized
  holder process: since a virtual pointer's held button releases on client
  disconnect, `left-mouse-down` forks a detached holder that keeps the
  connection (and the held button) alive until `left-mouse-up` signals it,
  coordinated by a profile-suffixed pidfile in `$XDG_RUNTIME_DIR`.
- **Keyboard input** (`key-sequence`, `type`, `hold-key`) via
  `zwp_virtual_keyboard_v1`. The required XKB keymap text is generated in-process
  (the wtype technique) so the binary never links libxkbcommon: sequential
  keycodes for four real modifier keys plus each requested keysym, with Unicode
  symbol names (`U597D`, `U1F600`) for CJK/emoji. The CU key-spec grammar parser
  and key-name tables are ported verbatim from `x11-bridge`.
- **Window queries** (`windows`, `frontmost-app`, `activate-window`) via
  `zwlr_foreign_toplevel_management_unstable_v1` - advertised by all three target
  compositors (Sway, Hyprland, and Niri; verified against niri's source), so
  `activate-window` works on all of them. `ext_foreign_toplevel_list_v1` is a
  list-only fallback for compositors that advertise only the ext protocol.
  Window info mirrors `x11-bridge`'s snake_case `WindowInfo`.
- **doctor** reports `WAYLAND_DISPLAY`, the detected compositor, and the bound
  version of every global the bridge depends on - the diagnostics backbone.
- **session-start / session-end** no-ops (`{"ok":true}`) for JS session parity.
- Pure-Rust, fully static musl builds for x86_64 and aarch64 (rust-lld,
  `+crt-static`); no libwayland, no libxkbcommon, no C dependencies.
- CI: fmt + clippy (`-D warnings`), build + test on both musl targets with a
  static-binary assertion, and a headless-sway end-to-end smoke test.

### Contract deviations from x11-bridge (documented in DESIGN.md)

- `WindowInfo.geometry` is always `{0,0,0,0}` - the foreign-toplevel protocols
  do not expose geometry.
- `app-under-point` returns `null` (no geometry to hit-test).
- `cursor-position` exits 1 - Wayland has no protocol to read the global pointer
  position without input focus (the JS executor uses Electron instead).
