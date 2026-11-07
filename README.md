# Emoji & Clipboard for XFCE

A lightweight Rust + GTK 3 emoji, kaomoji, clipboard and GIF picker for Manjaro XFCE on X11. It aims for a Windows 11-style workflow without Electron or an embedded browser.

## Features

- **Emoji:** Bundled Unicode emoji data from the `emojis` crate, with names, shortcodes and supported skin-tone variants. Search the bundled dataset.
- **Kaomoji and text art:** Includes faces and text designs such as `:)`, `:D`, `.__.`, `¯\_(ツ)_/¯` and table flips.
- **Clipboard history:** Keeps up to 50 recent text entries locally. Click an entry to copy/type it into the previously focused X11 window.
- **GIPHY GIF search:** Search the GIPHY library, see small previews, and click a result to download a compact animated GIF rendition to your local library and copy it as `image/gif`.
- **Local GIF library:** Import GIF files from disk and reuse them later, even without an internet connection or a GIPHY key.
- **Win + ; hotkey:** A small background instance is started at login. The XFCE shortcut toggles the picker through a local Unix socket at `~/.local/share/emoji-clipboard/instance.sock` (independent of `XDG_RUNTIME_DIR` and `XDG_DATA_HOME`), so the shortcut and autostart process use the same socket even if their `XDG_RUNTIME_DIR` differs.
- **GIPHY analytics:** Uses GIPHY's anonymous `random_id` and sends its documented view/click/send tracking events. No name, email, or account identifier is requested by this app.
- **Low-overhead UI:** Rust + GTK 3, no web engine, only 12 online GIF results per search, previews are size-limited, and network calls run on worker threads so the UI remains responsive.

## Appearance

Use the **☾ Dark / ☀ Light** button in the top bar to switch between dark and light themes. Your choice is saved in `~/.local/share/emoji-clipboard/config.json` and restored the next time the picker opens. Existing config files are upgraded automatically; their missing theme setting defaults to light mode.

## Important Linux behavior

1. **X11 required:** The global hotkey, focus capture and synthetic text insertion use XFCE/X11 and `xdotool`. Wayland sessions do not provide the same behavior.
2. **Emoji and text insertion:** The app copies selected text to the clipboard and then asks `xdotool` to type it into the window that was focused before the picker opened. If an app blocks synthetic typing, paste with `Ctrl+V`.
3. **GIF clipboard support:** GIFs are copied with `xclip -selection clipboard -t image/gif`. The receiving application must support GIF clipboard data. Some apps only accept image files or static PNG data. If animated GIF paste is unsupported, the downloaded file is still saved in the local GIF library.
4. **No tray icon in this version:** The background process is controlled by Win+; and `emoji-clipboard --quit`. This avoids the unavailable/deprecated GTK tray API that caused the previous build failure.
5. **Clipboard privacy:** Text history is stored in `~/.local/share/emoji-clipboard/history.json`, capped at 50 entries. Avoid copying passwords or other secrets while history is enabled. Use **Clipboard → Clear history** to erase it.
6. **GIPHY privacy:** Searching sends the exact search text and your API key to GIPHY. The app also requests a GIPHY anonymous random ID and sends GIPHY's view/click/send analytics pings for online GIF results. Your API key and anonymous ID are saved locally in `~/.local/share/emoji-clipboard/config.json` with Unix file mode `0600`. Alternatively, set `GIPHY_API_KEY` in the environment before starting the app.
7. **API usage:** GIPHY beta keys are limited to 100 API calls per hour. Search is limited to 50 characters and the app requests 12 results per search. Production access may require GIPHY review and pricing approval.

## Install on Manjaro XFCE

Open a terminal in the extracted project directory and run:

```bash
chmod +x install.sh
./install.sh
```

The installer installs `rust`, `cargo`, `gtk3`, `pkgconf`, `xdotool`, `xclip` and `noto-fonts-emoji` with `pacman`, builds an optimized release binary, installs it under `~/.local/bin`, creates an XFCE autostart entry and assigns **Win+;**. If another command already owns that exact key, the installer backs up its old command to `~/.config/emoji-clipboard-previous-super-semicolon.txt` before assigning the picker. Cargo.toml includes the GIPHY networking dependencies `ureq` and `urlencoding`; Cargo downloads them during the build. If you are updating an older extracted folder, extract this archive into a fresh folder instead of mixing old and new source files.

If the hotkey still does not work, run this from the extracted project directory:

```bash
chmod +x fix-hotkey.sh
./fix-hotkey.sh
```

The updated repair script rebuilds and installs the Rust binary, restarts the background process, verifies the IPC socket, and registers **Super+semicolon** through XFCE's `xfconf-query` settings. You can also add this command manually in **Settings Manager → Keyboard → Application Shortcuts**:

```bash
$HOME/.local/bin/emoji-clipboard --toggle
```

Then press **Super+semicolon** (Windows key + semicolon) when XFCE asks for the shortcut.

## Why GIPHY instead of Tenor?

Google stopped accepting new Tenor API sign-ups on January 13, 2026 and shut down the Tenor API on June 30, 2026. New installations therefore need another provider; this project uses GIPHY. See the [official Tenor API discontinuation notice](https://support.google.com/tenor/answer/10455265).

## Configure GIPHY GIF search

Online GIF search needs a GIPHY API key. The app does not include a shared key.

1. Create an account and a beta API key in the [GIPHY Developer Dashboard](https://developers.giphy.com/dashboard/).
2. Open the picker with **Win+;**.
3. Open the **GIFs** tab and click **API Key**.
4. Paste the key and save it.
5. Search for a term such as `happy`, `cat` or `hello`. Click a result to download its compact animated rendition and copy it to the clipboard.

GIPHY beta keys are currently limited to 100 API calls per hour. The app asks for a G-rated search result set and displays the required **Powered By GIPHY** attribution. It also registers the documented view, click and send analytics events. See the [GIPHY API documentation](https://developers.giphy.com/docs/api/) and [API terms](https://developers.giphy.com/docs/api/).

The API key can also be provided through the `GIPHY_API_KEY` environment variable. A non-empty environment value takes precedence over the saved key.

## Local GIF collection

Use **GIFs → Add GIF**, or copy `.gif` files into:

```text
~/.local/share/emoji-clipboard/gifs/
```

Click **My GIFs** to return to the local collection after an online search. The thumbnails are static previews; the saved GIF files remain animated.

## Run manually

```bash
# Open the picker
emoji-clipboard

# Start hidden in the background
emoji-clipboard --background

# Toggle the running instance
emoji-clipboard --toggle

# Quit the background instance
emoji-clipboard --quit
```

## Uninstall

```bash
./uninstall.sh
```

The uninstall script removes the binary, XFCE autostart entry, app-menu entry and this app's hotkey (only if it still points to this app). It deliberately keeps the local GIFs, GIPHY configuration and clipboard history. Remove `~/.local/share/emoji-clipboard/` manually to delete those files too.

## Build manually

The current release is **0.3.4**. It includes the required `ureq` and `urlencoding` dependencies, GTK 3 callback/dialog compatibility fixes, persistent dark mode, and the stable IPC socket path used by the Win+; hotkey.


```bash
sudo pacman -S --needed rust cargo gtk3 pkgconf xdotool xclip noto-fonts-emoji
cargo build --release
./target/release/emoji-clipboard
```

## Notes

- The emoji page renders up to 240 results at a time so older PCs are not overloaded; search to narrow the full bundled dataset.
- Emoji glyph appearance depends on the installed emoji font, such as Noto Color Emoji.
- Clipboard history can contain sensitive text. Clear it when needed.
- If the shortcut does not work, run `./fix-hotkey.sh` from the extracted project folder; then test `~/.local/bin/emoji-clipboard --toggle` from a terminal.
- GIPHY analytics are performed in background threads so tracking requests do not block the picker.

## License

MIT. See `LICENSE`.


## Fixing Win+; if the old binary says `Unknown option --toggle`

That message means an older executable is still installed. From the extracted project folder, run:

```bash
chmod +x fix-hotkey.sh
./fix-hotkey.sh
```

The updated script first runs `cargo build --release`, verifies the new `emoji-clipboard 0.3.4` binary, installs it to `~/.local/bin/emoji-clipboard`, restarts the background process, verifies `~/.local/share/emoji-clipboard/instance.sock`, and registers the XFCE shortcut. It will fail with a clear message if Cargo, GTK 3 development files, or `xfconf-query` are missing. If startup fails, inspect `~/.local/share/emoji-clipboard/launcher.log` (or the matching `XDG_DATA_HOME` location).


## 0.3.4 startup fix

GTK/GIO's `ApplicationExtManual::run()` forwards the original process arguments to GApplication. Since this app parses its own `--background`, `--toggle`, and `--quit` flags, passing them through caused GApplication to print `Unknown option --background` and exit before the picker started. Version 0.3.4 uses `run_with_args()` with only the executable name after handling the app's own flags. The repair script rebuilds the binary, checks both flags in `--help`, clears the old startup log, removes a stale socket after stopping the old process, and starts the background instance again.


## 0.3.4 dark mode

Version 0.3.4 adds a top-bar theme toggle. Dark mode uses a dedicated GTK 3 CSS palette for the picker, notebook tabs, search fields, buttons, GIF cards, clipboard history and dialogs. The light/dark selection is saved alongside the GIPHY settings; older config files remain readable and default to light mode.
