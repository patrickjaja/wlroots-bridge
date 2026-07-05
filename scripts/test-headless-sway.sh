#!/usr/bin/env bash
# Run wlroots-bridge subcommands against a throwaway headless sway compositor.
#
# sway's headless backend (WLR_BACKENDS=headless) starts a full wlroots
# compositor with no physical output/input devices, advertising exactly the
# wlr protocols this bridge depends on (virtual_pointer, virtual_keyboard,
# screencopy, foreign_toplevel, xdg_output). That makes it the one avenue for
# end-to-end testing on a machine with no Wayland session (e.g. an X11 host or
# CI). The input-injection paths (type / key / click) would otherwise move the
# real cursor and type into the focused window, so they must run against this
# isolated compositor - never a live session.
#
# Usage:
#   scripts/test-headless-sway.sh                 # run the full smoke sequence
#   WLROOTS_BRIDGE_BIN=path scripts/test-headless-sway.sh
#
# Requires: sway, plus this repo's binary (built or pointed at via
# WLROOTS_BRIDGE_BIN). On ubuntu-24.04 `apt-get install sway` provides it.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN="${WLROOTS_BRIDGE_BIN:-target/x86_64-unknown-linux-musl/release/wlroots-bridge}"
if [ ! -x "$BIN" ]; then
  # Fall back to a native debug/release build if the musl one is absent.
  for cand in target/release/wlroots-bridge target/debug/wlroots-bridge; do
    [ -x "$cand" ] && BIN="$cand" && break
  done
fi
if [ ! -x "$BIN" ]; then
  echo "error: wlroots-bridge binary not found (build it or set WLROOTS_BRIDGE_BIN)" >&2
  exit 1
fi
BIN="$(readlink -f "$BIN")"

if ! command -v sway >/dev/null 2>&1; then
  echo "error: sway not found (install sway; on Ubuntu: apt-get install -y sway)" >&2
  exit 1
fi

# Isolated runtime dir + a private wayland display name so we never touch a
# real session.
export XDG_RUNTIME_DIR="$(mktemp -d)"
chmod 700 "$XDG_RUNTIME_DIR"
export WAYLAND_DISPLAY="wlroots-bridge-test-$$"
# Headless backend, no real devices, software rendering (llvmpipe) for CI.
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman
export SWAYSOCK="$XDG_RUNTIME_DIR/sway-ipc.sock"
# Minimal sway config: one headless output, a terminal we can type into if
# present, and exit after we're done (we kill it explicitly).
SWAY_CONFIG="$XDG_RUNTIME_DIR/sway.conf"
cat >"$SWAY_CONFIG" <<'EOF'
output HEADLESS-1 resolution 1280x800 position 0 0
exec sleep 600
EOF

cleanup() {
  [ -n "${SWAY_PID:-}" ] && kill "$SWAY_PID" 2>/dev/null || true
  rm -rf "$XDG_RUNTIME_DIR" 2>/dev/null || true
}
trap cleanup EXIT

echo ">>> starting headless sway on $WAYLAND_DISPLAY"
sway -c "$SWAY_CONFIG" >"$XDG_RUNTIME_DIR/sway.log" 2>&1 &
SWAY_PID=$!

# Wait for the wayland socket to appear.
for _ in $(seq 1 50); do
  [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
  sleep 0.2
done
if [ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; then
  echo "error: sway did not create $WAYLAND_DISPLAY" >&2
  cat "$XDG_RUNTIME_DIR/sway.log" >&2 || true
  exit 1
fi

fail=0
run() {
  echo ">>> $BIN $*"
  if ! "$BIN" "$@"; then
    echo "!!! FAILED: $*" >&2
    fail=1
  fi
}

# --- doctor: must report the wlr globals as present ---
echo ">>> doctor"
DOCTOR="$("$BIN" doctor)"
echo "$DOCTOR"
for g in virtual_pointer virtual_keyboard screencopy xdg_output; do
  if ! echo "$DOCTOR" | grep -q "\"$g\":[0-9]"; then
    echo "!!! doctor: $g not advertised by headless sway" >&2
    fail=1
  fi
done

# --- screens: at least one output with the configured geometry ---
echo ">>> screens"
SCREENS="$("$BIN" screens)"
echo "$SCREENS"
echo "$SCREENS" | grep -q '"width":1280' || { echo "!!! screens: expected 1280 wide" >&2; fail=1; }
echo "$SCREENS" | grep -q '"height":800' || { echo "!!! screens: expected 800 tall" >&2; fail=1; }

# --- screenshot: must return base64 + plausible dimensions ---
echo ">>> screenshot"
SHOT="$("$BIN" screenshot)"
echo "$SHOT" | head -c 120; echo " ...(truncated)"
echo "$SHOT" | grep -q '"base64":"' || { echo "!!! screenshot: no base64" >&2; fail=1; }
# 1280x800 = 1.024M px < cap, so the emitted size should match native.
echo "$SHOT" | grep -q '"width":1280' || { echo "!!! screenshot: width not 1280" >&2; fail=1; }

# --- zoom: a region crop ---
run zoom --x 100 --y 100 --w 200 --h 150

# --- input synthesis (harmless in the isolated compositor) ---
run pointer-move --x 640 --y 400
run pointer-click --x 640 --y 400 --button left --count 1
run pointer-scroll --x 640 --y 400 --dy 3
run key-sequence --keys 'ctrl+a'
run type --text 'hello wlroots'
run hold-key --key shift --duration-ms 50

# --- held-button pair: down then up (the holder-process path) ---
run left-mouse-down
sleep 0.3
run left-mouse-up

# --- windows / frontmost (may be empty in a bare headless session) ---
echo ">>> windows"
"$BIN" windows || echo "(windows failed - tolerated if no toplevel manager)"
echo ">>> frontmost-app"
"$BIN" frontmost-app || echo "(frontmost-app returned no app - tolerated)"

# --- session lifecycle no-ops ---
run session-start
run session-end

if [ "$fail" -ne 0 ]; then
  echo "=== SMOKE TEST FAILED ===" >&2
  cat "$XDG_RUNTIME_DIR/sway.log" >&2 || true
  exit 1
fi
echo "=== headless-sway smoke test PASSED ==="
