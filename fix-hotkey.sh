#!/usr/bin/env bash
set -Eeuo pipefail

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/emoji-clipboard"
BUILD_BIN="$PROJECT_DIR/target/release/emoji-clipboard"
KEY="<Super>semicolon"
PROP="/commands/custom/$KEY"
COMMAND="$BIN --toggle"
CHANNEL="xfce4-keyboard-shortcuts"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
LOG_DIR="$DATA_HOME/emoji-clipboard"
SOCKET_DIR="$HOME/.local/share/emoji-clipboard"
SOCKET="$SOCKET_DIR/instance.sock"
LOG="$LOG_DIR/launcher.log"

fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || fail "Cargo is missing. Install it with: sudo pacman -S --needed rust cargo"
command -v pkg-config >/dev/null 2>&1 || fail "pkg-config is missing. Install it with: sudo pacman -S --needed pkgconf"
pkg-config --exists gtk+-3.0 || fail "GTK 3 development files are missing. Install them with: sudo pacman -S --needed gtk3"
command -v xfconf-query >/dev/null 2>&1 || fail "xfconf-query is missing. Install it with: sudo pacman -S --needed xfconf"

printf 'Repairing Emoji & Clipboard v0.3.4 from: %s\n' "$PROJECT_DIR"
cd "$PROJECT_DIR"
cargo build --release
[[ -x "$BUILD_BIN" ]] || fail "Cargo finished but $BUILD_BIN was not created."

"$BUILD_BIN" --help | grep -q -- "--background" || fail "The built binary does not advertise --background; source/build folder may be outdated."
"$BUILD_BIN" --help | grep -q -- "--toggle" || fail "The built binary does not advertise --toggle; source/build folder may be outdated."

BUILT_VERSION="$("$BUILD_BIN" --version 2>&1 || true)"
[[ "$BUILT_VERSION" == "emoji-clipboard 0.3.4" ]] || fail "Unexpected build output: $BUILT_VERSION"

# Stop any stale copy only after the replacement binary has built successfully.
if command -v pkill >/dev/null 2>&1; then
  pkill -u "$(id -u)" -x emoji-clipboard >/dev/null 2>&1 || true
fi
sleep 0.3

mkdir -p "$BIN_DIR" "$LOG_DIR" "$SOCKET_DIR"
install -m 755 "$BUILD_BIN" "$BIN"

# Register or update the XFCE shortcut, preserving any previous custom command.
mkdir -p "$HOME/.config"
if CURRENT="$(xfconf-query --channel "$CHANNEL" --property "$PROP" 2>/dev/null)"; then
  if [[ "$CURRENT" != "$COMMAND" ]]; then
    printf '%s\n' "$CURRENT" > "$HOME/.config/emoji-clipboard-previous-super-semicolon.txt"
    xfconf-query --channel "$CHANNEL" --property "$PROP" --set "$COMMAND"
    echo "Updated the existing Super+; binding (previous command backed up)."
  else
    echo "Super+; is already assigned to Emoji & Clipboard."
  fi
else
  xfconf-query --channel "$CHANNEL" --property "$PROP" --create --type string --set "$COMMAND"
  echo "Created the Super+; binding."
fi

# Start the newly installed app hidden and capture startup errors for diagnosis.
rm -f "$SOCKET"
: > "$LOG"
nohup "$BIN" --background >>"$LOG" 2>&1 </dev/null &

for _ in $(seq 1 50); do
  if [[ -S "$SOCKET" ]]; then
    break
  fi
  sleep 0.1
done

[[ -S "$SOCKET" ]] || {
  echo "The background app did not create its IPC socket. The startup log follows:" >&2
  echo "Startup log: $LOG" >&2
  tail -n 40 "$LOG" >&2 2>/dev/null || true
  exit 1
}

INSTALLED_VERSION="$("$BIN" --version 2>&1 || true)"
printf '\nInstalled: %s\n' "$INSTALLED_VERSION"
printf 'Socket:    %s\n' "$SOCKET"
printf 'Shortcut:  Super+semicolon (Windows key + ;)\n'
printf 'Command:   %s\n' "$COMMAND"
printf '\nNow press Win+; to open the picker. To test from a terminal, run:\n  "%s" --toggle\n' "$BIN"
