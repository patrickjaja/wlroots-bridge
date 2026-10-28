# wlroots-bridge - design & contract (v0.1, experimental)

First-party wlroots-Wayland bridge for Claude Desktop's Computer Use on Linux.
It replaces the shell-out chain (`ydotool` for input, `grim` for screenshots,
`hyprctl` / `swaymsg`+`jq` / `niri msg` for window enumeration) on
**wlroots-based Wayland sessions** (Sway, Hyprland, Niri) with a single
pure-Rust, statically-linkable binary. It is the direct sibling of `x11-bridge`
(X11 / XWayland) and `kwin-portal-bridge` (KDE/Wayland) and speaks the **same
JSON contract**, so the JS executor treats them interchangeably.

- **Invoked by:** `js/cu_linux_executor.js` in `claude-desktop-extra`, via
  `execFileSync(bin, [subcommand, ...args])`, reading `JSON.parse(stdout.trim())`.
  The binary resolves from `$WLROOTS_BRIDGE_BIN` / `resourcesPath` / `$PATH`
  (wiring the resolver into the JS side is a separate workstream).
- **One-shot:** every subcommand is a short-lived process. No daemon, no
  persistent portal session (see [No-daemon rationale](#no-daemon-rationale)).
  The **one** exception is the `left-mouse-down` holder process (see
  [Held-button mechanism](#held-button-mechanism)).
- **Output:** exactly one JSON object/array on stdout. Errors: a human message
  on stderr and **exit code 1**. clap parse errors exit 2.

## Technology (pure Rust, Smithay ecosystem)

`wayland-client` (its `client_rust` backend - **no libwayland**),
`wayland-protocols` (`xdg_output`, `ext_foreign_toplevel_list`),
`wayland-protocols-wlr` (`virtual_pointer`, `screencopy`, `foreign_toplevel`),
`wayland-protocols-misc` (`virtual_keyboard`). Plus `image` (JPEG), `xkeysym`,
`clap`, `serde`/`serde_json`, `base64`, `anyhow`. **No libxkbcommon** - the XKB
keymap the virtual keyboard needs is generated as text and compiled by the
compositor (see [Keymap generation](#keymap-generation)). The result is a fully
static musl binary that runs on every distro without runtime dependencies.

## Coordinate system

All coordinates are **Wayland global logical pixels** - the same space the JS
sends (`Math.round(x)`, `Math.round(y)`). A multi-monitor layout is one logical
plane; a monitor's `geometry.x/y` is its offset within it (from `xdg-output`
`logical_position`), which can be negative for an output left of or above
`(0,0)`. Absolute pointer motion maps a global logical coordinate into the
layout's bounding box (see [Pointer input](#pointer-input)). The `screencopy`
buffer is in an output's **physical** pixels and untransformed panel
orientation; capture undoes the frame's y-invert flag, then applies the
`wl_output.transform` (all eight: normal, 90, 180, 270, flipped, flipped-90/180/270,
following wlroots `wlr_box_transform`), so screenshots match the output's
logical orientation and the size `screens` reports. `zoom` then scales the
requested logical region by the image/logical ratio before cropping, so
fractional-scaled outputs crop correctly. The reported `scale` is the
transform-oriented mode width over the logical width (a rotated unscaled output
reports 1.0).

Wayland has **no "primary" output**. We designate the output at logical `(0,0)`
as primary/active; if none sits there, the first enumerated output. Both flags
(`is_primary` / `is_active`) are set on that one output so the JS `isPrimary`
display picker resolves unambiguously.

## Subcommand -> JSON output contract

Shapes mirror `x11-bridge` (`src/output.rs`) 1:1 and are verified against the JS
reader in `cu_linux_executor.js`. Field-name casing is **not uniform** and must
be preserved: screenshots/app-refs are camelCase; screens/windows are
snake_case.

| Subcommand | Output shape | JS reader (evidence) |
|---|---|---|
| `doctor` | contract v1 keys (see below) plus, when the compositor is reachable, `{wayland_display, compositor, globals:{virtual_pointer, virtual_keyboard, screencopy, foreign_toplevel_wlr, foreign_toplevel_ext, xdg_output, wl_output_count, wl_seat, wl_shm}}` | always exit 0 |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary}]` | `mapRustScreenToDisplay` reads `geometry.*`, `scale`, `is_primary`, `name`, `id` (-> `_bridgeId`) |
| `screenshot [--display N]` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY}` (camelCase) | `screenshotResultFromRust` reads `result.displayWidth`/`displayHeight`/`originX`/`originY` |
| `zoom --x --y --w --h [--display N]` | `{base64, width, height}` | `zoom` reads `result.base64`/`width`/`height` |
| `windows` | `[{id, title, geometry, pid, desktop_file_name, resource_class, resource_name, window_role, window_type, is_dock, is_desktop, is_visible, is_minimized, is_normal_window, is_dialog, transient, transient_for, output, stacking_order, is_active, exclude_from_capture, keep_above}]` (snake_case) | `_appRefFromWindow` / `openApp` read `desktop_file_name`, `resource_class`, `is_minimized`, `is_visible`, `is_active`, `is_dock`, `is_desktop`, `title`, `id` |
| `cursor-position` | **error, exit 1** (unqueryable) | `getCursorPosition` uses Electron `getCursorScreenPoint()` instead |
| `frontmost-app` | `{bundleId, displayName}` (camelCase) | `_appRefFromCommand` reads `result.bundleId`/`displayName` |
| `app-under-point --x --y` | `null` (best-effort unsupported) | `_appRefFromCommand` tolerates null |
| `activate-window --window <id>` | `{activated: <id>}` | `activateWindow` ignores the body |
| `pointer-move --x --y` | `{action, x, y}` (camelCase) | body ignored |
| `pointer-click --x --y [--button] [--count] [--modifier ...]` | `{action, x, y}` | body ignored |
| `pointer-scroll --x --y [--dx] [--dy]` | `{action, x, y}` | body ignored |
| `pointer-drag --from-x --from-y --to-x --to-y` | `{action, fromX, fromY, toX, toY}` | body ignored |
| `left-mouse-down` / `left-mouse-up` | `{action, button, isHeld}` | body ignored |
| `key-sequence --keys <spec> [--repeat N]` | `{action, keys, repeat?}` | body ignored |
| `type --text <t> [--delay-ms N]` | `{action, text, charCount}` | body ignored |
| `hold-key --key <k>... --duration-ms N` | `{action, keys, durationMs}` | body ignored |
| `session-start [--foreground]` | `{ok:true, session:"noop"}` | `execBridge` tolerates any/empty output |
| `session-end` | `{ok:true, ended:true}` | same |

## `--display` selection

`--display <name>` matches an output's `name` (the `wl_output` connector name,
e.g. `eDP-1`, `DP-2`, or the `xdg-output` name as a fallback). The JS passes
`display._bridgeId`, which is the `id` field from `screens` (= the output name).
Default (no flag): the output at logical `(0,0)`, else the first.

## Key-spec grammar

Accepted by `key-sequence` (and each `--key` of `hold-key`). **Identical to
x11-bridge** (the parser + `CU_KEY_NAMES` / `MODIFIER_TOKENS` tables are ported
verbatim from `x11-bridge/src/input/keymap.rs`, which extracted them from
`_mapKey` in `cu_linux_executor.js`):

```
spec     := token ("+" token)*
token    := modifier | keyname
modifier := ctrl | control | alt | shift | super | meta | cmd | command
keyname  := <CU key-name from table> | <single literal char>
```

- Case-insensitive. The base key is the last non-modifier token.
- **Modifier aliases:** `cmd`, `command`, `meta` normalize to **`super`**;
  `control` -> `ctrl`.
- CU key-name table (CU token -> X11 keysym name): `enter`/`return`->`Return`,
  `backspace`->`BackSpace`, `delete`->`Delete`, `escape`/`esc`->`Escape`,
  `tab`->`Tab`, `space`/` `->`space`, arrows, `home`/`end`,
  `pageup`/`page_up`->`Prior`, `pagedown`/`page_down`->`Next`,
  `capslock`->`Caps_Lock`. Anything else passes through as a keysym by char/name.

## New virtual devices

Every command creates its virtual pointer or keyboard afresh, and the device
lives only as long as the process. Sway advertises a seat's pointer and
keyboard capabilities only while some device provides them; with no physical
input (headless, nested) the new device is what adds the capability, and
clients bind `wl_pointer` / `wl_keyboard` only after they see it, so events sent
in the same burst as the device's creation reach nobody. Before creating a
device the bridge reads `wl_seat.capabilities` (`Conn::seat_has`); after
creating it (and, for the keyboard, uploading the keymap) it does a roundtrip
and, only if the capability was absent before, waits 50 ms
(`Conn::settle_new_device`). A seat with a keyboard and pointer costs one extra
roundtrip per device. In the per-char path of `type` (more than 200 distinct
glyphs) the earlier keyboards stay alive until the process exits, so only the
first one waits.

## Keymap generation

The `zwp_virtual_keyboard_v1` protocol requires uploading an XKB keymap fd
before sending keycodes; the compositor compiles it. To avoid linking
libxkbcommon (which would break the pure-Rust static build), we **generate the
XKB keymap text ourselves** (the wtype technique, hand-rolled in
`src/input/keymap.rs::generate_keymap`):

- We lay out sequential XKB keycodes: four fixed modifier keys (Control, Shift,
  Alt, Super with proper `modifier_map` entries), then one keycode per requested
  keysym.
- Each requested keysym is bound at **both** shift levels of its keycode to the
  exact glyph, so a stray Shift never corrupts it. Modifiers are sent both
  ways: as key events on their real keycodes, and, after every modifier press
  or release, as `modifiers()` with the depressed real-mod mask (Shift `0x1`,
  Control `0x4`, Mod1 `0x8`, Mod4 `0x40`, matching the generated
  `modifier_map`). wtype sends only `modifiers()`. wlroots (and niri on smithay
  0.7) passes virtual-keyboard keys on without updating its modifier state
  from them, so without the mask `ctrl+c` reaches the client as a plain `c`.
- The mask is compositor state that smithay does not reset when the device is
  destroyed, so `hold-key` and `pointer-click --modifier` catch SIGTERM,
  SIGINT and SIGHUP (`src/input/signals.rs`): the signal ends the wait early,
  the normal release path runs (keys up, mask 0, roundtrip) and the process
  exits `128 + signal`. SIGKILL cannot be caught.
- Named keys use their X keysym name (`Return`, `space`, `F5`); Latin-1 glyphs
  their name (`eacute`) or `U00E9`; CJK/emoji the Unicode symbol name
  (`U597D`, `U1F600`) via the keysym encoding `0x01000000 | codepoint`.
- The `key` request carries the **Linux evdev code** = XKB keycode - 8.
- `type` builds one keymap holding every *distinct* char in the text (repeated
  chars reuse a keycode), so a whole string needs a single upload. Texts with
  more than 200 distinct glyphs page to a per-char keymap (correct, slower).

The keymap text is written to a `memfd` (falling back to an unlinked
`$XDG_RUNTIME_DIR` file) and uploaded with a NUL terminator, as xkbcommon
expects.

## Pointer input

`zwlr_virtual_pointer_manager_v1.create_virtual_pointer` (no seat hint - the
compositor assigns the default seat). Absolute motion uses
`motion_absolute(time, x, y, x_extent, y_extent)`. wlroots' `wlr_cursor` maps
the normalized position onto the output layout's **bounding box, including a
negative origin** (`lx = box.x + x / x_extent * box.w`). We therefore send
`(x - min_x, y - min_y)` with extents `(max_x - min_x, max_y - min_y)` over all
outputs' logical rectangles, clamped to `[0, extent - 1]`. That makes a single
global coordinate land exactly on any monitor, including ones left of / above
`(0,0)`, vertically offset, or separated by a gap; creating the pointer
per-output would restrict motion to one monitor. Every absolute positioning
(move, click, scroll, drag) goes through this mapping. Every event group is
closed with `frame()`.

- **Buttons:** evdev codes `BTN_LEFT=0x110`, `BTN_MIDDLE=0x112`,
  `BTN_RIGHT=0x111`; press+frame, release+frame.
- **Scroll:** `axis` with ~15.0 units per notch. Positive `dy` = down, positive
  `dx` = right (matching the KDE / x11-bridge convention).
- **Drag:** press, ~20 interpolated `motion_absolute` steps ~8 ms apart,
  release (mirrors x11-bridge).
- **Click modifiers** (`--modifier ctrl`): held on a virtual keyboard for the
  duration of the click burst, released after.

### Held-button mechanism

`left-mouse-down` / `left-mouse-up` are the only stateful pair. A virtual
pointer's held buttons are **released the instant the client disconnects**, so a
one-shot process cannot leave a button held. We therefore implement
`left-mouse-down` by **daemonizing a holder process**:

1. The command forks; the child `setsid()`s to detach.
2. The child opens its **own fresh Wayland connection** (proxies are not
   fork-safe), presses `BTN_LEFT` + frame, and writes its PID to a pidfile in
   `$XDG_RUNTIME_DIR` (`wlroots-bridge-lmb[-<profile>].pid`, profile-suffixed so
   multi-profile Desktop instances don't collide).
3. It then blocks on a `SIGTERM`-driven loop, servicing the connection so the
   held button stays alive.
4. The parent returns immediately with `{isHeld:true}`.

`left-mouse-up` reads the pidfile and sends `SIGTERM`; the holder releases the
button (its own disconnect also releases it as a backstop), removes the pidfile,
and exits. Both commands are idempotent: a stale/dead pidfile is ignored, and
releasing when nothing is held is not an error.

## Windows

`zwlr_foreign_toplevel_management_unstable_v1` lists toplevels with `app_id` /
`title` / `state` and supports `activate`; `activate-window` uses its `activate`
request with a `wl_seat`. **All three target compositors advertise it** - Sway,
Hyprland, and Niri (verified against niri's source: `src/protocols/foreign_toplevel.rs`
implements the `zwlr_foreign_toplevel_manager_v1` global with an `activate`
handler, in addition to `ext_foreign_toplevel_list_v1`). So on Sway/Hyprland/Niri
this bridge takes the wlr path and `activate-window` works everywhere.

The `ext_foreign_toplevel_list_v1` (staging) path is kept as a **fallback for
list-only compositors** that advertise the ext protocol but not the wlr manager.
That protocol has no `activate` (so `activate-window` errors on such a
compositor) and no state (windows report neutral state). It is not the path any
of the three primary compositors actually take, but it broadens coverage.

**Contract deviations (wlroots-specific):**

- Neither protocol exposes **geometry**, so every `WindowInfo.geometry` is
  `{0,0,0,0}`. Documented; the JS window helpers that matter
  (`_appRefFromWindow`, `openApp` matching) key off `app_id`/`title`/state, not
  geometry.
- `app-under-point` therefore returns **null** (best-effort unsupported) - there
  is no geometry to hit-test.
- `resource_class` and `desktop_file_name` are both set to the `app_id`;
  `resource_name` is null. `bundle_id` derivation strips a trailing `.desktop`.
- `exclude_from_capture` is always `false` (no wlroots equivalent).
- `id` is the foreign-toplevel handle's protocol object id as a decimal string
  (or, for the ext protocol, its stable `identifier` when provided).
- `frontmost-app` picks the `activated` toplevel, else the first.

**Niri note:** niri gates `wlr-screencopy`, `virtual-pointer`, `virtual-keyboard`,
and the foreign-toplevel protocols as **privileged** - it disables them for
clients that connect through a Wayland security-context (sandboxed clients). The
bridge runs as an ordinary client (launched by Claude Desktop, not inside a
sandbox), so it gets full access; a future sandboxed deployment would see these
globals disappear from the registry, which `doctor` would surface.

## cursor-position

**Not queryable on Wayland**: no protocol exposes the global pointer position
without input focus. The command exits 1 with a clean error. The JS executor
uses Electron's `getCursorScreenPoint()` on this path anyway, so this is never a
functional gap.

## CLI contract v1

The bridge implements the Computer Use bridge CLI contract v1
(<https://github.com/patrickjaja/claude-desktop-extra/blob/master/docs/computer-use-bridge-contract.md>).

- `doctor` always exits 0. Required keys: `contract:1`, `bridge:"wlroots-bridge"`,
  `version`, `backend:"wlroots"`, `session_match`, `backend_reachable`,
  `backend_error`, `session_supported:false`, `restore_token_present:false`,
  `restore_token_path:null`, `daemon_running:false`, `session_socket:null`.
  `session_match` is true when `WAYLAND_DISPLAY` is set, the connection works and
  the compositor advertises `zwlr_virtual_pointer_manager_v1`,
  `zwp_virtual_keyboard_manager_v1` and `zwlr_screencopy_manager_v1`. The
  desktop app additionally gates its wlroots tier on `SWAYSOCK` /
  `HYPRLAND_INSTANCE_SIGNATURE` / `NIRI_SOCKET`; the bridge does not, so a
  protocol-capable compositor outside those three still reports a match.
- `session-start` -> `{"ok":true,"session":"noop"}`; `session-end` ->
  `{"ok":true,"ended":false,"daemon":"none"}`; `forget-session` ->
  `{"ok":true,"ended":false,"daemon":"none","token_removed":false,"token_path":null}`.
  There is no daemon and no grant, so none of them has side effects.
- Every coordinate flag accepts negative values (layouts can start left of or
  above the origin).
- `tests/contract.rs` runs these checks against the binary without a compositor.

## Error contract

- Success: one JSON value on stdout, exit 0.
- Failure: `anyhow` error chain printed to stderr (`{error:#}`), exit 1.
- Unknown/invalid CLI: clap message on stderr, exit 2.

## No-daemon rationale

Unlike `kwin-portal-bridge` (which runs a long-lived `session-start` daemon to
keep a RemoteDesktop/ScreenCast portal grant + PipeWire stream alive), the
wlroots virtual-input and screencopy protocols are **stateless per request** and
need no prior grant. Every subcommand opens a fresh connection, does its work,
and exits. `session-start`/`session-end` are no-ops that report `{ok:true}` to
satisfy the JS session bookkeeping. The single stateful exception is the
`left-mouse-down` holder process described above.

## Explicitly out of scope for v0.1 (matches x11-bridge)

- **`open-app` / `list-installed-apps` / `get-app-icon`** - the JS executor
  resolves and launches `.desktop` entries portably in-process.
- **`read-clipboard` / `write-clipboard`** - the Electron `clipboard` API is
  used directly by the JS in non-Cowork mode; a `wlr-data-control` path is
  deferred.
- **Teach overlays**, **capture-exclusion / set-window-geometry / keep-above** -
  compositor-specific niceties with no portable wlroots path in v0.1.

## Future work

- **`ext_image_copy_capture`**: some compositors advertise it alongside (or
  instead of) `wlr-screencopy`. v0.1 only implements `wlr-screencopy` (Sway,
  Hyprland, and Niri all ship it). Adding the ext protocol would broaden
  compositor coverage.
- **GNOME-Wayland** is covered by a separate `gnome-portal-bridge` (in
  development), not this binary.
