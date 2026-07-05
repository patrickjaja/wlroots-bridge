# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
