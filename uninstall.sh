#!/usr/bin/env bash
set -euo pipefail

BIN="$HOME/.local/bin/emoji-clipboard"
KEY="<Super>semicolon"
PROP="/commands/custom/$KEY"
SHORTCUT_COMMAND="$BIN --toggle"

if [[ -x "$BIN" ]]; then
  "$BIN" --quit >/dev/null 2>&1 || true
fi

if command -v xfconf-query >/dev/null 2>&1; then
  CURRENT="$(xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" 2>/dev/null || true)"
  if [[ "$CURRENT" == "$SHORTCUT_COMMAND" ]]; then
    xfconf-query --channel xfce4-keyboard-shortcuts --property "$PROP" --reset >/dev/null 2>&1 || true
  fi
fi

rm -f "$BIN"
rm -f "$HOME/.config/autostart/emoji-clipboard.desktop"
rm -f "$HOME/.local/share/applications/emoji-clipboard.desktop"

echo "Emoji & Clipboard was uninstalled. Your GIF library, settings and clipboard history were kept at:"
echo "  $HOME/.local/share/emoji-clipboard"
echo "The GIPHY API key and anonymous analytics ID are stored in config.json in that folder (mode 600)."
echo "Remove that folder manually if you also want to delete your saved GIFs and history."
