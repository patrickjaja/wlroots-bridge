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
| `doctor` | contract v1 keys (see below) plus, when the compositor is reachable, `{wayland_display, compositor, globals:{virtual_pointer, virtual_keyboard, screencopy, foreign_toplevel_wlr, foreign_toplevel_ext, xdg_output, wl_output_count, wl_seat, wl_shm}}`, and always `abortWatch:{mechanism, available, reason}` | always exit 0 |
| `abort-watch` | long-running, JSON lines (see "abort-watch") | optional addition |
| `screens` | `[{id, name, geometry:{x,y,width,height}, scale, refresh_millihz, is_active, is_primary}]` | `mapRustScreenToDisplay` reads `geometry.*`, `scale`, `is_primary`, `name`, `id` (-> `_bridgeId`) |
| `screenshot [--display N] [--keep-window ID]... [--mask-all]` | `{base64, width, height, displayWidth, displayHeight, displayId, originX, originY, mask?}` (camelCase) | `screenshotResultFromRust` reads `result.displayWidth`/`displayHeight`/`originX`/`originY` |
| `zoom --x --y --w --h [--display N] [--keep-window ID]... [--mask-all]` | `{base64, width, height, mask?}` | `zoom` reads `result.base64`/`width`/`height` |
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

- Keys sit at their US pc105 positions where the keysym has one: Escape on
  evdev 1 (KEY_ESC), Control_L 29, Shift_L 42, Alt_L 56, Super_L 125 (with
  `modifier_map` entries), letters, digits, punctuation (an uppercase or
  shifted glyph takes its key's code when the lowercase one is not in the
  same keymap), Return, Tab, BackSpace, space, F1-F12, arrows, Home/End,
  Prior/Next, Insert, Delete. Everything else goes to codes the US layout
  leaves unmapped, then to the alphanumeric block, then to the rest, never to
  xkb 9 or a code that carries a modifier or lock in the US layout, and never
  above 255 (X11 clients). A keysym requested twice reuses its code.
- Why: Hyprland (seen on 0.56.2) matches a virtual keyboard's keys against
  its key BINDINGS by evdev code in the user's own layout, not by the keymap
  the client uploaded. The previous sequential layout put Control_L on evdev
  1, so every Ctrl the bridge injected fired the user's Escape binds (and
  would have aborted Computer Use on the global-bind fallback); single keys
  sat on evdev 5 (`4`). Applications are not affected either way: they read
  the uploaded keymap, where every placed code carries exactly the requested
  keysym (checked with `xkbcli-interactive-wayland` in
  `tests/abort_sway.rs`). On non-US user layouts Hyprland may still see a
  different symbol for letters and punctuation (e.g. QWERTZ y/z); Escape,
  the modifiers and the named keys are layout-independent.
- Shift levels. `type` and combos without Shift: each requested keysym is
  bound at **both** shift levels of its keycode to the exact glyph, so a
  stray Shift never corrupts it (`type 'aB'` gives `a`, `B`). A
  `key-sequence` / `hold-key` combo that holds Shift: a key at its US
  position carries the US pair (`[ a, A ]`, `[ 1, exclam ]`,
  `[ Tab, ISO_Left_Tab ]`), so Shift selects the shifted symbol as on a real
  keyboard: `shift+a` gives `A`, `shift+1` gives `!`, `ctrl+shift+a` gives
  `A` with Control (^A), `shift+tab` gives `ISO_Left_Tab` (what toolkits and
  terminals expect for back-tab). Requesting the shifted glyph itself
  (`shift+A`) still gives `A`; keys without a US position (`é`, CJK) keep both
  levels equal. Until this change Shift never changed the symbol
  (`shift+a` gave `a`). Modifiers are sent both
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

- Neither protocol exposes **geometry**. On sway and Hyprland it comes from the
  compositor IPC (see "Window geometry"); elsewhere every `WindowInfo.geometry`
  is `{0,0,0,0}`.
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

## Window geometry (sway, Hyprland)

The foreign-toplevel protocols carry no geometry, so `windows` reads the
compositor IPC (`src/ipc.rs`): sway `GET_TREE`, Hyprland `j/clients` +
`j/monitors`. A window gets `geometry` (global logical px, the space `screens`
uses), `output` (set only when it names a `screens` id), `pid`, `is_visible` and
an IPC-ordered `stacking_order` when its app id + title pair is unique among the
toplevels AND among the IPC windows. Two windows with the same app id and title
stay without geometry - never guessed. XWayland windows match on their X class
(what both compositors report as the toplevel app id). niri, river and others
keep the protocol-only answer (`geometry` `{0,0,0,0}`, `output` null).

- Rects: sway container `rect` (borders included, title/tab bar excluded);
  Hyprland `at`/`size` (client box, border excluded).
- `is_visible`: sway `visible` (false for hidden tabs, other workspaces,
  scratchpad, under a fullscreen view); Hyprland: mapped, not `hidden` (group
  member), on an active or open special workspace, or pinned.
- `stacking_order`: back-to-front rank. Matched windows: tiled < floating
  (sway: floating list order; Hyprland: focus recency, special workspace above
  regular) < fullscreen; every unmatched window ranks below them in
  enumeration order.

## Mask mode (`screenshot` / `zoom`)

`--keep-window <id>` (repeatable; ids from `windows`) keeps the visible parts of
those windows and paints everything else #1E1E1E: desktop, bars, other windows,
Claude's own window. `--mask-all` paints the whole image (no IPC needed). The
result gains `mask: {mode: "keep"|"all", kept: [{id, appId, title, visible}],
regions: [{x,y,width,height}]}` so the caller can check every kept id still
named the window it listed (ids are per-connection protocol ids and can shift
when a window opens or closes in between).

A kept window's region is its IPC rect minus every other visible window that
may be above it (only a tiled window on the same workspace under a floating or
fullscreen kept window is known to be below; floating-over-floating is always
cut), minus Hyprland layer surfaces on the `top`/`overlay` levels, minus the
four `decoration:rounding` corner squares on Hyprland. The regions are mapped
onto the captured image, which is already in logical orientation (the output
transform is undone first), with exact integer math, rounding kept areas
inward, so a partly covered pixel is masked (1.25/1.5 scales included).

The IPC is read before and after the copy; the frame is used only when both
give the same plan (one retry). Fails closed, no image: no IPC, an unknown id,
a kept window that shares its app id + title with a window that is not kept, a
kept window with no unambiguous IPC match, or a visible window with no
geometry. A kept window that is not visible keeps nothing (fully masked image,
`visible:false`).

Not masked when drawn over a kept window (`doctor.screenshot_mask_gaps`):
popups/menus of any app (not in either IPC), on sway also layer-shell
`top`/`overlay` surfaces (notifications) and unmanaged XWayland windows (X
menus, tooltips). A kept app's own popups are masked where they extend beyond
its rect. Also inherent: a translucent kept window shows what is beneath it, and
Hyprland reports animation goals, so a window mid-animation is masked at its
final position.

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
- Additive `doctor` key `abortWatch` (optional contract addition): `{mechanism:"sway"|"hyprland"|null,
  available, reason, stale, skippedKeyboards, consumes}`. Short read-only IPC
  requests only (no subscription, no `eval`), bounded at 1.5 s; unavailable
  with a reason when the user binds a bare Escape or the compositor has no
  runtime binding API.
- Additive `doctor` keys: `window_geometry` (sway/Hyprland IPC answered and a
  foreign-toplevel protocol is advertised), `screenshot_mask` (`window_geometry`,
  the full mask IPC read succeeded and screencopy is advertised) and
  `screenshot_mask_gaps` (see "Mask mode"). All false/empty elsewhere.
- Every coordinate flag accepts negative values (layouts can start left of or
  above the origin).
- `tests/contract.rs` runs these checks against the binary without a compositor.

## abort-watch (optional contract addition)

Long-running Escape watcher. The protocol half (stdout `ready` / `abort` /
`ack` / `error` lines, stdin `suspend` / `resume`, signals, parent death, exit
codes 0 / 1 / 2, `SharedBinding`) is the `cu-abort-watch` crate; `src/abort/`
is the compositor half. The backend follows the HOST compositor:
`SWAYSOCK` that accepts a connection -> sway, else `HYPRLAND_INSTANCE_SIGNATURE`
-> Hyprland, else an `error` event and exit 2 (nothing created): "no runtime
binding API on <niri | first XDG_CURRENT_DESKTOP entry | this compositor>", or
"SWAYSOCK is set but sway does not answer".

Rule for every step: a compositor reply that is not exactly the documented
success (sway `success: true` per command, Hyprland `ok`, a bind that then
shows up in `j/binds`, a known `j/status` shape) undoes what was done and
exits 2. Unverified means not armed.

Escape and applications: sway has no non-consuming `bindsym`, so while armed
a physical Escape is consumed by sway and the focused app does not get it.
Hyprland binds are non-consuming: Escape aborts AND reaches the app.
`doctor.abortWatch.consumes` reports this (`true` sway, `false` Hyprland
0.55+, `null` when the version cannot be told).

### sway (`src/abort/sway.rs`)

- Physical keyboard = `GET_INPUTS` entry with `type: keyboard` that is not an
  injector: name `wlr_virtual_keyboard_v1` (every `zwp_virtual_keyboard_v1`
  client, this bridge included; identifier `0:0:wlr_virtual_keyboard_v1`),
  `virtual keyboard` (older wlroots), or a name containing `ydotool`.
  Remappers that carry the user's keys through uinput (keyd, kanata) count as
  physical.
- Bind, one per keyboard, batched with `; `:
  `mode default bindsym --input-device=<id> --no-repeat --inhibited Escape nop claude-cu-abort`.
  Unbind: `mode default unbindsym --input-device=<id> --no-repeat --inhibited Escape`.
  The `mode default` prefix puts the bind into the default mode from any mode
  without a `mode` event. Fallbacks, learned on the first bind and kept in the
  record: without `--no-repeat` when sway rejects it (repeat is then
  debounced), without the prefix when sway rejects it AND the current mode is
  `default` (otherwise arming fails).
- Identifiers with characters sway's command parser treats specially (`;` `,`
  `'` `$` braces `#` ...): `bindsym "--input-device=<id>" ...` without the
  prefix (sway 1.12 keeps quotes literally inside `mode default ...`), so
  only while sway is in the default mode; otherwise on the next `mode` event
  to `default`. Identifiers with `"` or `\` are never bound (sway keeps
  backslash escapes literally inside quotes, tested on 1.12): logged, listed
  in `doctor.abortWatch.skippedKeyboards`, and arming fails when no physical
  keyboard can be bound.
- Events (`SUBSCRIBE ["binding","input","mode","workspace","shutdown"]`, made
  before binding): `binding` with command `nop claude-cu-abort` -> abort;
  `input` `added` physical keyboard -> bound (once across holders); `mode`
  `default` -> keyboards still missing are bound; `workspace` `reload` -> the
  user-binding check runs again, then everything is re-bound (an identical
  `bindsym` overwrites, so every holder doing it is harmless); a reload that
  brings a user Escape binding empties the record (sway already dropped our
  binds) and ends the watcher (error, exit 1); `shutdown` or a closed socket
  -> error, exit 1.
- Mode quirk (sway 1.12, measured): an Escape in a mode whose Escape binding
  switches to `default` (the stock `resize` mode) produces: mode change ->
  binding run of the user's binding -> binding run of ours (sway evaluates the
  same press again in `default`). A binding event of ours within 100 ms after
  a mode change or another Escape binding run is dropped, so leaving `resize`
  with Escape does not abort.
- User binding check: sway has no request that lists bindings. `GET_CONFIG`
  returns the main config text (includes not expanded); `src/abort/swayconf.rs`
  re-reads it like sway: `\` continuations, `#` comments, `{ }` blocks,
  `set $var`, `include` (sway variables, `~`, `$ENV` of the sway process from
  `/proc/<pid>/environ` via `SO_PEERCRED`, `*`/`?` globs, relative to the
  including file; regular files only, so a FIFO cannot hang arming; the whole
  read has a 2 s deadline). A `bindsym` of a bare `Escape` (or `0xff1b`) /
  `bindcode 9` that lands in the `default` mode and is not removed by a later
  `unbindsym`/`unbindcode` blocks arming. Not seen: bindings added at runtime
  with `swaymsg bindsym`, and include files this process cannot read
  (logged).
- A user binding with OUR signature (`--input-device=<id> --inhibited
  Escape`; sway's `binding_key_compare` ignores `--no-repeat`) would be
  removed by our unbind. Every unbind (last holder, stale cleanup) skips such
  ids. The first holder clears stale binds BEFORE refusing for a user
  binding, because a stale device bind would shadow the user's own Escape.

### Hyprland (`src/abort/hypr.rs`)

All Hyprland strings live in `hypr::cmd`.

- Dialect: `j/status` (read-only; an `eval` probe would clear the user's
  config error list: `eval` runs `m_errors.clear()`).
- With a Lua config, arming and disarming (and every re-bind) go through
  `eval`, which clears the config-error list Hyprland shows the user
  (verified on 0.56.2). There is no other runtime bind API in Lua mode, so
  this is unavoidable: a config error the user has not fixed yet disappears
  from the error bar when Computer Use starts or stops (it comes back on the
  next config reload). hyprlang (`keyword`) does not do this. `configProvider: lua`
  -> Lua; `hyprlang` -> hyprlang 0.55/0.56; the exact reply `unknown request`
  -> hyprlang 0.54 or older; anything else -> not armed. Note that every Lua
  bind/unbind `eval` still clears that error list (unavoidable: `eval` is the
  only runtime bind API in Lua mode).
- Physical keyboard = `j/devices` `keyboards[].name` not starting with
  `hl-virtual-keyboard` (Hyprland's name for every virtual keyboard, plus
  `-<process>` with `misc:name_vk_after_proc` and `-N` for duplicates) or
  `virtual-keyboard`, and not containing `ydotool`.
- Lua bind (option keys of `hlBind` identical at v0.55.0, v0.56.0, v0.57.2):
  `eval local t = __claude_cu_abort or {}` / `__claude_cu_abort = t` /
  `t[#t + 1] = hl.bind("EsCaPe", hl.dsp.event("claude-cu-abort"), { non_consuming = true, dont_inhibit = true, description = "claude-cu-abort", device = { inclusive = true, list = { "<kbd>", ... } } })`.
  Unbind: `eval for _, b in ipairs(__claude_cu_abort or {}) do if tostring(b) ~= "HL.Keybind(expired)" then pcall(function() b:remove() end) end end` / `__claude_cu_abort = nil`.
  The `tostring` guard is load-bearing: when something else removed our bind
  (a user's `hl.unbind("escape")` lower-cases and so removes ours too, or a
  config tool), the stored object is expired, and on 0.55/0.56 `remove()` on
  an expired object dereferences null (`getKeybindFromUserdata` returns an
  always-set optional): Hyprland crashes with SIGSEGV (reproduced on 0.56.2),
  and `pcall` cannot catch that. `__tostring` is null-safe in 0.55, 0.56 and
  0.57, so it is the only method ever called on a stored object before
  `remove()`; every Lua removal (disarm, stale cleanup, heal, device re-sync,
  re-bind after a run-time removal) goes through this one string.
  On 0.55/0.56 `remove()` erases every bind with the same key STRING and
  mods (0.57 removes only the object), hence the `EsCaPe` spelling: the
  user's `escape` binds in submaps are never hit. `hl.unbind(...)` is never
  used (it normalizes case).
- hyprlang bind (single-character flags in any order; `n` non-consuming, `p`
  bypass inhibitors, `k` per-device (0.55+), `d` description):
  `keyword bindnpkd , EsCaPe, <kbd> <kbd>, claude-cu-abort, event, claude-cu-abort`;
  on `bind: invalid flag` (no `k`, 0.54 and older):
  `keyword bindnpd , EsCaPe, claude-cu-abort, event, claude-cu-abort`, then
  `keyword bindnd , ...` if `p` is unknown too (global, `syntheticSafe:
  false`). Unbind: `keyword unbind , EsCaPe` (exact key string: ours only).
- Confirmation: after the first bind the bind must appear in `j/binds`
  (description / `event` arg `claude-cu-abort`), else undo and exit 2.
- socket2: `custom>>claude-cu-abort` -> abort; `configreloaded>>` -> when our
  bind is gone from `j/binds`, re-check user binds, then re-bind (not while
  any holder suspends the global bind). No hot-plug event exists, and nothing
  reports a bind removed at run time (a user's `hl.unbind("escape")`,
  `keyword unbind`, a config tool), so every 2 s (by elapsed time: steady
  socket2 traffic does not starve it) `j/devices` and `j/binds` are re-read:
  when the keyboard list changed or our bind is missing, the guarded unbind
  runs (clearing the Lua table) and the bind is made again.
- Messages name a conflicting user bind by its `description` when it has one
  (every Lua bind reports its dispatcher as `__lua <ref>`).
- `suspend` / `resume` act only on the global bind and are counted across
  holders in `<instance>-suspended`: the first suspender unbinds, the last
  resumer binds again. A watcher that exits while suspended resumes first.
- User binding check: `j/binds` (live): a bind that is not ours, modmask 0,
  key `escape` (any case) or keycode 9, in the global submap or
  `submap_universal`; plus, from the plain-text `binds` (only 0.57+ prints
  `flags:`), an Escape bind with `ignore_mods` at any modmask. Before 0.57
  `ignore_mods` is not visible.

### Shared binds

One holder set per compositor INSTANCE:
`$XDG_RUNTIME_DIR/claude-cu-esc/sway/i<fnv64 of the canonical SWAYSOCK path>/`
and `.../hyprland/<instance signature>/` (hashed when it is not a plain file
name), created through `SharedBinding::for_mechanism` (checks `claude-cu-esc`
and `<mechanism>`) and `SharedBinding::in_dir` (checks the instance dir). Two
sways (second VT, nested) or two Hyprlands of one user never share holders.

Used through `lock()`: the first holder clears stale binds when `pruned() > 0`
or a record exists, checks the user's bindings, writes the record (intent),
registers, then binds (on failure: undone, unregistered). The record
`bound.json` holds the spelling variant and the bound devices; the last
holder's `release` unbinds from it and deletes it (keeps it, listing what is
left, when a quoted sway identifier could not be unbound outside the default
mode). Event-thread actions (hot-plug, reload, poll, mode) take the lock and
do nothing once this holder is no longer registered. Every IPC request has a
1.5 s timeout; the event subscription blocks without a timeout and is shut
down by `disarm`.

Heal: `abort-watch` with stdin already at EOF arms (as first holder when the
others are dead: stale binds and record cleared), then disarms and exits 0.
`doctor.abortWatch.stale` is true when the instance directory has a record or
holder files but no live holder (read-only check: taking the lock would prune
the holder files the next first holder needs; a reused pid counts as live).

Presses within 1 s of the previous event are one press (debounce), covering
key repeat without `--no-repeat` and duplicate binds left by a crash.

### To verify on a real Hyprland (VM), worst wrong assumption first

0. Never crash the compositor (seen on 0.56.2 before the guard: SIGSEGV,
   Hyprland restarted in safe mode): arm; run `hyprctl eval
   'hl.unbind("escape")'` (removes our bind too); `tostring(__claude_cu_abort[1])`
   is `HL.Keybind(expired)`; within 2 s the watcher logs "removed at run
   time" and binds again; disarm; Hyprland keeps running. Repeat with
   disarm right after the `hl.unbind` (before the poll). 0.55: same.
1. Removal stays ours (a wrong assumption deletes the user's Escape binds):
   with a user `bind = , escape, submap, reset` inside a submap (both
   dialects), arm and disarm, then `hyprctl binds -j` still lists it. Lua
   0.55/0.56: confirm `remove()` matches the stored key string `EsCaPe`
   exactly; 0.57: only the object.
2. Non-consuming (a wrong assumption takes Escape from apps): a physical
   Escape closes a dialog in the focused app AND produces
   `custom>>claude-cu-abort`; same with an app that inhibits shortcuts
   (`dont_inhibit` / `p`).
3. Virtual keyboard name (a wrong assumption lets the model's Escape abort
   the turn): `hyprctl devices -j` while `wlroots-bridge key-sequence` runs
   shows `hl-virtual-keyboard...`; `key-sequence --keys escape` produces NO
   `custom>>claude-cu-abort`; no physical keyboard name starts with it.
4. Device scoping works (a wrong assumption = no abort at all): the
   inclusive `list` / hyprlang device list matches `devices` names; the
   `EsCaPe` key resolves to Escape.
5. `j/status` shape per version (`configProvider` `lua` / `hyprlang`; 0.54:
   `unknown request`); otherwise abort-watch refuses to arm.
6. Lua: the multi-line `eval` answers `ok`; `__claude_cu_abort` survives
   between `eval` requests; what `hyprctl reload` does to the binds and the
   Lua global (stale and reload paths).
7. hyprlang 0.55/0.56: `keyword bindnpkd` accepted with a space-separated
   device list; 0.54: `bindnpkd` answers `bind: invalid flag`, `bindnpd` and
   `keyword unbind , EsCaPe` work.
8. `j/binds` reports our bind with `description: claude-cu-abort` (arm
   confirmation) and `submap_universal` as a string; 0.57 text `binds` has
   the `flags:` line with `ignore_mods`.
9. `configreloaded>>` arrives on socket2 after `hyprctl reload`.
10. Bridge key codes (keymap change): `key-sequence --keys ctrl+a` reaches
    the app as ^A and fires NO Escape bind (neither one scoped to the
    bridge's virtual keyboard nor an unscoped one); `key-sequence --keys
    escape` fires no bind scoped to a physical keyboard; `type` with shifted
    and non-ASCII text (`Hi! é 好 😀`) arrives unchanged.
11. Still untested on a VM: an app that inhibits shortcuts (`dont_inhibit`),
    keyboard hot-plug, Hyprland 0.54 (global fallback, `unknown request`
    status), 0.55 and 0.57 specifics, the hyprlang physical-key path.

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
