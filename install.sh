#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
AUTOSTART_DIR="$HOME/.config/autostart"
BIN="$BIN_DIR/emoji-clipboard"
KEY="<Super>semicolon"
PROP="/commands/custom/$KEY"
SHORTCUT_COMMAND="$BIN --toggle"

if ! command -v pacman >/dev/null 2>&1; then
  echo "This installer is intended for Manjaro/Arch Linux (pacman)." >&2
  exit 1
fi

printf '\nInstalling build and runtime dependencies...\n'
sudo pacman -S --needed rust cargo gtk3 pkgconf xdotool xclip noto-fonts-emoji

printf '\nBuilding the optimized Rust app v0.3.4...\n'
cd "$PROJECT_DIR"
cargo build --release

BUILD_BIN="$PROJECT_DIR/target/release/emoji-clipboard"
BUILD_VERSION="$("$BUILD_BIN" --version 2>&1 || true)"
if [[ "$BUILD_VERSION" != "emoji-clipboard 0.3.4" ]]; then
  echo "Unexpected build output: $BUILD_VERSION" >&2
  exit 1
fi
"$BUILD_BIN" --help | grep -q -- "--background" || { echo "This build does not support --background." >&2; exit 1; }
"$BUILD_BIN" --help | grep -q -- "--toggle" || { echo "This build does not support --toggle." >&2; exit 1; }

# Stop an older process only after the new binary has built successfully.
if command -v pkill >/dev/null 2>&1; then
  pkill -u "$(id -u)" -x emoji-clipboard >/dev/null 2>&1 || true
fi
sleep 0.3

mkdir -p "$BIN_DIR" "$APP_DIR" "$AUTOSTART_DIR"
install -m 755 "$BUILD_BIN" "$BIN"

cat > "$AUTOSTART_DIR/emoji-clipboard.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Emoji & Clipboard
Comment=Windows-style emoji and clipboard picker for XFCE
Exec="$BIN" --background
Icon=face-smile
Terminal=false
OnlyShowIn=XFCE;
X-GNOME-Autostart-enabled=true
DESKTOP

cat > "$APP_DIR/emoji-clipboard.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Emoji & Clipboard
Comment=Windows-style emoji and clipboard picker for XFCE
Exec="$BIN"
Icon=face-smile
Terminal=false
Categories=Utility;
DESKTOP

if command -v xfconf-query >/dev/null 2>&1; then
  if xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" >/dev/null 2>&1; then
    CURRENT="$(xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" 2>/dev/null || true)"
    if [[ "$CURRENT" != "$SHORTCUT_COMMAND" ]]; then
      printf '%s\n' "$CURRENT" > "$HOME/.config/emoji-clipboard-previous-super-semicolon.txt"
      xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" --set "$SHORTCUT_COMMAND"
      echo "Updated the existing Win+; binding (previous command backed up)."
    else
      echo "Win+; shortcut is already configured."
    fi
  else
    xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" \
      --create --type string --set "$SHORTCUT_COMMAND"
    echo "Configured Win+; to toggle Emoji & Clipboard."
  fi
else
  echo "xfconf-query not found. Run ./fix-hotkey.sh from XFCE after installing xfconf."
fi

# Start the background process now; the autostart entry starts it on later logins.
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
LOG_DIR="$DATA_HOME/emoji-clipboard"
SOCKET_DIR="$HOME/.local/share/emoji-clipboard"
SOCKET="$SOCKET_DIR/instance.sock"
mkdir -p "$LOG_DIR" "$SOCKET_DIR"
rm -f "$SOCKET"
: > "$LOG_DIR/launcher.log"
nohup "$BIN" --background >>"$LOG_DIR/launcher.log" 2>&1 </dev/null &
for _ in $(seq 1 50); do
  [[ -S "$SOCKET" ]] && break
  sleep 0.1
done
if [[ ! -S "$SOCKET" ]]; then
  echo "The app did not start its background IPC server. See: $LOG_DIR/launcher.log" >&2
  tail -n 40 "$LOG_DIR/launcher.log" >&2 2>/dev/null || true
  exit 1
fi

printf '\nDone.\n'
echo "  Hotkey:       Win+;"
echo "  Background:   starts hidden and waits for Win+;"
echo "  GIF folder:   $HOME/.local/share/emoji-clipboard/gifs"
echo "  App history:  $HOME/.local/share/emoji-clipboard/history.json"
echo "  GIPHY key:    configure from GIFs > API Key"
echo "  Open app:     $BIN"
echo ""
echo "If Win+; still does not open the picker, run: ./fix-hotkey.sh"
