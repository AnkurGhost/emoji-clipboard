use gdk_pixbuf::Pixbuf;
use gtk::gdk;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_ID: &str = "io.github.emoji_clipboard";
const MAX_VISIBLE: usize = 240;
const MAX_HISTORY: usize = 50;
const MAX_CLIPBOARD_CHARS: usize = 20_000;
const MAX_GIPHY_RESULTS: usize = 12;
const MAX_GIF_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
const MAX_GIF_PREVIEW_BYTES: u64 = 350 * 1024;

#[derive(Clone)]
struct EmojiItem {
    glyph: String,
    name: String,
    shortcode: String,
}

#[derive(Clone)]
struct KaomojiItem {
    value: String,
    name: String,
}

#[derive(Serialize, Deserialize, Default)]
struct StoredData {
    history: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct AppConfig {
    #[serde(default)]
    giphy_api_key: String,
    #[serde(default)]
    giphy_customer_id: String,
    // Missing values in older config.json files default to the existing light theme.
    #[serde(default)]
    dark_mode: bool,
}

#[derive(Clone)]
struct RemoteGif {
    id: String,
    title: String,
    preview_bytes: Option<Vec<u8>>,
    full_url: String,
    analytics_onload: Option<String>,
    analytics_onclick: Option<String>,
    analytics_onsent: Option<String>,
}

#[derive(Deserialize)]
struct GiphySearchResponse {
    #[serde(default)]
    data: Vec<GiphyApiGif>,
}

#[derive(Deserialize)]
struct GiphyApiGif {
    id: String,
    #[serde(default)]
    title: String,
    images: GiphyImages,
    #[serde(default)]
    analytics: Option<GiphyAnalytics>,
}

#[derive(Default, Deserialize)]
struct GiphyImages {
    original: Option<GiphyApiImage>,
    downsized: Option<GiphyApiImage>,
    downsized_large: Option<GiphyApiImage>,
    fixed_width_small: Option<GiphyApiImage>,
    preview_gif: Option<GiphyApiImage>,
}

#[derive(Deserialize)]
struct GiphyApiImage {
    url: String,
}

#[derive(Default, Deserialize)]
struct GiphyAnalytics {
    onload: Option<GiphyAnalyticsEvent>,
    onclick: Option<GiphyAnalyticsEvent>,
    onsent: Option<GiphyAnalyticsEvent>,
}

#[derive(Deserialize)]
struct GiphyAnalyticsEvent {
    url: String,
}

#[derive(Deserialize)]
struct GiphyRandomIdResponse {
    data: GiphyRandomIdData,
}

#[derive(Deserialize)]
struct GiphyRandomIdData {
    random_id: String,
}

enum GifWorkerEvent {
    SearchReady { query: String, gifs: Vec<RemoteGif>, customer_id: String },
    SearchFailed { query: String, message: String },
    DownloadReady { path: PathBuf, analytics_url: Option<String>, customer_id: String },
    DownloadFailed(String),
}

#[derive(Debug)]
enum AppCommand {
    Toggle,
    Quit,
}

struct AppPaths {
    data_dir: PathBuf,
    history_file: PathBuf,
    config_file: PathBuf,
    gif_dir: PathBuf,
    socket_file: PathBuf,
}

fn user_data_dir() -> PathBuf {
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join("emoji-clipboard");
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".local/share/emoji-clipboard")
}

fn socket_path() -> PathBuf {
    // Put IPC in a HOME-based location that is identical even if XFCE's
    // autostart and keyboard-shortcut launchers receive different XDG vars.
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".local/share/emoji-clipboard/instance.sock")
}

fn app_paths() -> AppPaths {
    let data_dir = user_data_dir();
    AppPaths {
        history_file: data_dir.join("history.json"),
        config_file: data_dir.join("config.json"),
        gif_dir: data_dir.join("gifs"),
        socket_file: socket_path(),
        data_dir,
    }
}

fn ensure_app_dirs(paths: &AppPaths) -> std::io::Result<()> {
    fs::create_dir_all(&paths.data_dir)?;
    fs::create_dir_all(&paths.gif_dir)?;
    Ok(())
}

fn load_config(path: &Path) -> AppConfig {
    let mut config = fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<AppConfig>(&json).ok())
        .unwrap_or_default();
    if let Ok(key) = env::var("GIPHY_API_KEY") {
        if !key.trim().is_empty() {
            config.giphy_api_key = key.trim().to_owned();
        }
    }
    config
}

fn save_config(path: &Path, config: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("Could not encode settings: {error}"))?;
    fs::write(path, json).map_err(|error| format!("Could not save settings: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect the API key file: {error}"))?;
    }
    Ok(())
}

fn send_command(command: &str, path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(mut stream) => stream.write_all(format!("{command}\n").as_bytes()).is_ok(),
        Err(_) => false,
    }
}

fn start_command_server(path: &Path, sender: Sender<AppCommand>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // A live socket always wins. Remove only a stale socket that cannot be contacted.
    if UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another Emoji Clipboard instance is already running",
        ));
    }
    let _ = fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    listener.set_nonblocking(true)?;
    let socket_to_remove = path.to_path_buf();

    thread::Builder::new()
        .name("emoji-clipboard-ipc".into())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_ok() {
                            let command = match line.trim() {
                                "toggle" => Some(AppCommand::Toggle),
                                "quit" => Some(AppCommand::Quit),
                                _ => None,
                            };
                            if let Some(command) = command {
                                if sender.send(command).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(90));
                    }
                    Err(_) => break,
                }
            }
            let _ = fs::remove_file(socket_to_remove);
        })?;
    Ok(())
}

fn load_history(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<StoredData>(&json).ok())
        .map(|data| data.history.into_iter().take(MAX_HISTORY).collect())
        .unwrap_or_default()
}

fn save_history(path: &Path, history: &[String]) {
    let payload = StoredData {
        history: history.to_vec(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&payload) {
        let temporary = path.with_extension("json.tmp");
        if fs::write(&temporary, json).is_ok() {
            let _ = fs::rename(temporary, path);
        }
    }
}

fn add_history(history: &mut Vec<String>, text: &str) -> bool {
    if text.trim().is_empty() || text.chars().count() > MAX_CLIPBOARD_CHARS {
        return false;
    }
    if history.first().map(String::as_str) == Some(text) {
        return false;
    }
    history.retain(|item| item != text);
    history.insert(0, text.to_owned());
    history.truncate(MAX_HISTORY);
    true
}

fn build_emoji_data() -> Vec<EmojiItem> {
    let mut data = Vec::new();
    let mut seen = HashSet::new();

    for emoji in emojis::iter() {
        if let Some(variants) = emoji.skin_tones() {
            for variant in variants {
                let glyph = variant.as_str().to_owned();
                if seen.insert(glyph.clone()) {
                    data.push(EmojiItem {
                        glyph,
                        name: variant.name().to_owned(),
                        shortcode: variant.shortcode().unwrap_or("").to_owned(),
                    });
                }
            }
        } else {
            let glyph = emoji.as_str().to_owned();
            if seen.insert(glyph.clone()) {
                data.push(EmojiItem {
                    glyph,
                    name: emoji.name().to_owned(),
                    shortcode: emoji.shortcode().unwrap_or("").to_owned(),
                });
            }
        }
    }
    data
}

fn build_kaomoji_data() -> Vec<KaomojiItem> {
    [
        (":)", "Smile"),
        (":-)", "Classic smile"),
        (":D", "Big smile"),
        ("xD", "Laughing"),
        (":P", "Tongue out"),
        (";)", "Wink"),
        (":3", "Cat smile"),
        (":|", "Straight face"),
        (":O", "Amazed"),
        (":(", "Sad"),
        (":'(", "Crying"),
        ("<3", "Heart"),
        ("^_^", "Happy"),
        ("^ω^", "Cute"),
        ("UwU", "UwU"),
        ("OwO", "OwO"),
        ("._.", "Blank face"),
        (".__.", "Tiny blank face"),
        (">_<", "Overwhelmed"),
        ("^.^", "Gentle smile"),
        ("T.T", "Tears"),
        ("QQ", "Crying"),
        ("-_-", "Unimpressed"),
        ("o_O", "Confused"),
        ("O_O", "Surprised"),
        ("T_T", "Crying"),
        ("ToT", "Tears"),
        ("(¬_¬)", "Side eye"),
        ("(ಥ﹏ಥ)", "Very sad"),
        ("(╥﹏╥)", "Sobbing"),
        ("(づ｡◕‿‿◕｡)づ", "Hug"),
        ("༼ つ ◕_◕ ༽つ", "Give a hug"),
        ("(っ˘з(˘⌣˘ )", "Kiss"),
        ("(づ￣ ³￣)づ", "Sending love"),
        ("ヽ(•‿•)ノ", "Yay"),
        ("٩(◕‿◕｡)۶", "Excited"),
        ("(ง'̀-'́)ง", "Fight"),
        ("(ง •̀_•́)ง", "Determined"),
        ("(ノಠ益ಠ)ノ彡┻━┻", "Flip table"),
        ("(╯°□°)╯︵ ┻━┻", "Flip table"),
        ("┬─┬ノ( º _ ºノ)", "Put table back"),
        ("(☞ﾟヮﾟ)☞", "Look over there"),
        ("☜(ﾟヮﾟ☜)", "Look back"),
        ("(•̀ᴗ•́)و", "You can do it"),
        ("(｡♥‿♥｡)", "In love"),
        ("(づ ◕‿◕ )づ", "Warm hug"),
        ("(っ•́｡•́)♪♬", "Music"),
        ("(￣▽￣)", "Relaxed"),
        ("(；一_一)", "Awkward"),
        ("(ಥ_ಥ)", "Tears"),
        ("(ᵔᴥᵔ)", "Bear"),
        ("ʕ•ᴥ•ʔ", "Bear"),
        ("(*^ω^*)", "Cute smile"),
        ("(*_*)", "Dizzy"),
        ("(=^･ω･^=)", "Cat"),
        ("(=ＴェＴ=)", "Crying cat"),
        ("(•ө•)♡", "Bird love"),
        ("¯\\_(ツ)_/¯", "Shrug"),
        ("¯\\_( ͡° ͜ʖ ͡°)_/¯", "Lenny shrug"),
        ("( ͡° ͜ʖ ͡°)", "Lenny face"),
        ("(☞ ͡° ͜ʖ ͡°)☞", "Lenny point"),
        ("༼ つ ಥ_ಥ ༽つ", "Need comfort"),
        ("(づ｡◕‿‿◕｡)づ ♡", "Love hug"),
        ("♡", "Heart"),
        ("ღ", "Heart 2"),
        ("✧˖°", "Sparkles"),
        ("(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", "Magic"),
        ("(•̀o•́)ง", "Let's go"),
    ]
    .iter()
    .map(|(value, name)| KaomojiItem {
        value: (*value).to_owned(),
        name: (*name).to_owned(),
    })
    .collect()
}

fn fetch_url_bytes(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => format!("Remote server returned HTTP {code}"),
            ureq::Error::Transport(error) => format!("Network error: {error}"),
        })?;

    if let Some(length) = response.header("Content-Length").and_then(|value| value.parse::<u64>().ok()) {
        if length > limit {
            return Err("The GIF is larger than the allowed download size.".into());
        }
    }

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read the download: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err("The GIF is larger than the allowed download size.".into());
    }
    Ok(bytes)
}

fn obtain_giphy_customer_id(api_key: &str, current_id: &str) -> Result<String, String> {
    if !current_id.trim().is_empty() {
        return Ok(current_id.trim().to_owned());
    }

    let url = format!(
        "https://api.giphy.com/v1/randomid?api_key={}",
        urlencoding::encode(api_key.trim())
    );
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => format!("GIPHY could not create an anonymous analytics ID (HTTP {code})."),
            ureq::Error::Transport(error) => format!("Could not reach GIPHY: {error}"),
        })?;
    let body = response
        .into_string()
        .map_err(|error| format!("Could not read GIPHY's ID response: {error}"))?;
    let parsed: GiphyRandomIdResponse = serde_json::from_str(&body)
        .map_err(|error| format!("GIPHY sent an unexpected ID response: {error}"))?;
    if parsed.data.random_id.trim().is_empty() {
        return Err("GIPHY returned an empty analytics ID.".into());
    }
    Ok(parsed.data.random_id)
}

fn search_giphy(
    query: &str,
    api_key: &str,
    existing_customer_id: &str,
) -> Result<(Vec<RemoteGif>, String), String> {
    if api_key.trim().is_empty() {
        return Err("Add your GIPHY API key using the API Key button first.".into());
    }
    let query = query.trim();
    if query.is_empty() {
        return Err("Type something to search for GIFs.".into());
    }
    if query.chars().count() > 50 {
        return Err("GIPHY search is limited to 50 characters.".into());
    }

    let customer_id = obtain_giphy_customer_id(api_key, existing_customer_id)?;
    let url = format!(
        "https://api.giphy.com/v1/gifs/search?api_key={}&q={}&limit={}&offset=0&rating=g&lang=en&customer_id={}",
        urlencoding::encode(api_key.trim()),
        urlencoding::encode(query),
        MAX_GIPHY_RESULTS,
        urlencoding::encode(&customer_id)
    );
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(12))
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => format!("GIPHY returned HTTP {code}. Check the API key and quota."),
            ureq::Error::Transport(error) => format!("Could not reach GIPHY: {error}"),
        })?;
    let body = response
        .into_string()
        .map_err(|error| format!("Could not read GIPHY's response: {error}"))?;
    let parsed: GiphySearchResponse = serde_json::from_str(&body)
        .map_err(|error| format!("GIPHY sent an unexpected response: {error}"))?;

    let mut gifs = Vec::new();
    for item in parsed.data.into_iter().take(MAX_GIPHY_RESULTS) {
        // Prefer a compact animated rendition for faster downloads on older PCs.
        let full_url = item.images.downsized.as_ref()
            .or(item.images.downsized_large.as_ref())
            .or(item.images.original.as_ref())
            .map(|image| image.url.clone());
        let Some(full_url) = full_url else { continue; };

        // GIPHY's preview_gif is deliberately small; fixed_width_small is a fallback.
        let preview_url = item.images.preview_gif.as_ref()
            .or(item.images.fixed_width_small.as_ref())
            .map(|image| image.url.clone())
            .unwrap_or_else(|| full_url.clone());
        let preview_bytes = fetch_url_bytes(&preview_url, MAX_GIF_PREVIEW_BYTES).ok();
        let title = if item.title.trim().is_empty() {
            format!("GIF {}", item.id)
        } else {
            item.title.trim().to_owned()
        };
        let analytics = item.analytics.unwrap_or_default();
        gifs.push(RemoteGif {
            id: item.id,
            title,
            preview_bytes,
            full_url,
            analytics_onload: analytics.onload.map(|event| event.url),
            analytics_onclick: analytics.onclick.map(|event| event.url),
            analytics_onsent: analytics.onsent.map(|event| event.url),
        });
    }
    Ok((gifs, customer_id))
}

fn ping_giphy_analytics(url: &str, customer_id: &str) {
    if url.trim().is_empty() || customer_id.trim().is_empty() {
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let separator = if url.contains('?') { '&' } else { '?' };
    let ping_url = format!(
        "{url}{separator}customer_id={}&ts={timestamp}",
        urlencoding::encode(customer_id)
    );
    let _ = ureq::get(&ping_url)
        .timeout(Duration::from_secs(5))
        .call();
}

fn send_giphy_analytics(url: Option<String>, customer_id: String, event_name: &'static str) {
    let Some(url) = url else { return; };
    let _ = thread::Builder::new()
        .name(format!("giphy-analytics-{event_name}"))
        .spawn(move || ping_giphy_analytics(&url, &customer_id));
}

fn download_giphy_gif(gif: &RemoteGif, gif_dir: &Path) -> Result<PathBuf, String> {
    let bytes = fetch_url_bytes(&gif.full_url, MAX_GIF_DOWNLOAD_BYTES)?;
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err("GIPHY did not return a valid GIF file.".into());
    }
    let safe_id = gif
        .id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .take(80)
        .collect::<String>();
    let file_name = if safe_id.is_empty() { "giphy-download.gif".to_owned() } else { format!("giphy-{safe_id}.gif") };
    let path = gif_dir.join(file_name);
    fs::write(&path, bytes).map_err(|error| format!("Could not save the GIF: {error}"))?;
    Ok(path)
}

fn capture_active_window() -> Option<String> {
    let output = Command::new("xdotool").arg("getactivewindow").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() { None } else { Some(id) }
}

fn type_into_previous_window(target: Option<String>, text: &str) {
    let Some(window_id) = target else {
        return;
    };
    // The clipboard is already populated. If typing fails, the user can still paste with Ctrl+V.
    let activated = Command::new("xdotool")
        .args(["windowactivate", "--sync", &window_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if activated {
        let _ = Command::new("xdotool")
            .args(["type", "--clearmodifiers", "--delay", "0", "--", text])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn choose_text(
    text: String,
    clipboard: &gtk::Clipboard,
    window: &ApplicationWindow,
    previous_window: &Rc<RefCell<Option<String>>>,
) {
    clipboard.set_text(&text);
    clipboard.store();
    let target = previous_window.borrow().clone();
    window.hide();
    type_into_previous_window(target, &text);
}

fn make_scroller() -> gtk::ScrolledWindow {
    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_shadow_type(gtk::ShadowType::None);
    scroller
}

fn clear_flow(flow: &gtk::FlowBox) {
    for child in flow.children() {
        flow.remove(&child);
    }
}

fn make_flow(columns: u32, homogeneous: bool) -> gtk::FlowBox {
    let flow = gtk::FlowBox::new();
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_homogeneous(homogeneous);
    flow.set_min_children_per_line(columns);
    flow.set_max_children_per_line(columns);
    flow.set_row_spacing(6);
    flow.set_column_spacing(6);
    flow.set_activate_on_single_click(true);
    flow
}

fn render_emoji_flow(
    flow: &gtk::FlowBox,
    info: &gtk::Label,
    items: &[EmojiItem],
    query: &str,
    choose: &Rc<dyn Fn(String)>,
) {
    clear_flow(flow);
    let normalized = query.trim().to_lowercase();
    let mut shown = 0usize;
    let mut matched = 0usize;
    for item in items.iter().filter(|item| {
        normalized.is_empty()
            || item.name.to_lowercase().contains(&normalized)
            || item.shortcode.to_lowercase().contains(&normalized)
            || item.glyph.contains(&normalized)
    }) {
        matched += 1;
        if shown >= MAX_VISIBLE {
            continue;
        }
        let button = gtk::Button::with_label(&item.glyph);
        button.set_size_request(48, 46);
        button.set_tooltip_text(Some(&format!("{}  :{}:", item.name, item.shortcode)));
        button.style_context().add_class("emoji-cell");
        let value = item.glyph.clone();
        let callback = choose.clone();
        button.connect_clicked(move |_| callback(value.clone()));
        flow.add(&button);
        shown += 1;
    }
    info.set_text(&format!(
        "{} emoji available  •  showing {}{}",
        items.len(),
        shown,
        if matched > MAX_VISIBLE { format!(" of {matched}; type to narrow the results") } else { String::new() }
    ));
    flow.show_all();
}

fn render_kaomoji_flow(
    flow: &gtk::FlowBox,
    items: &[KaomojiItem],
    query: &str,
    choose: &Rc<dyn Fn(String)>,
) {
    clear_flow(flow);
    let normalized = query.trim().to_lowercase();
    for item in items.iter().filter(|item| {
        normalized.is_empty()
            || item.name.to_lowercase().contains(&normalized)
            || item.value.to_lowercase().contains(&normalized)
    }) {
        let button = gtk::Button::with_label(&item.value);
        button.set_size_request(150, 48);
        button.set_tooltip_text(Some(&item.name));
        button.style_context().add_class("art-cell");
        let value = item.value.clone();
        let callback = choose.clone();
        button.connect_clicked(move |_| callback(value.clone()));
        flow.add(&button);
    }
    flow.show_all();
}

fn render_history_flow(
    flow: &gtk::FlowBox,
    history: &[String],
    query: &str,
    choose: &Rc<dyn Fn(String)>,
) {
    clear_flow(flow);
    let normalized = query.trim().to_lowercase();
    for item in history.iter().filter(|item| normalized.is_empty() || item.to_lowercase().contains(&normalized)) {
        let preview = item
            .replace('\n', "  ↵  ")
            .replace('\r', "")
            .chars()
            .take(110)
            .collect::<String>();
        let button = gtk::Button::with_label(&preview);
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        button.set_tooltip_text(Some(item));
        button.style_context().add_class("history-cell");
        let value = item.clone();
        let callback = choose.clone();
        button.connect_clicked(move |_| callback(value.clone()));
        flow.add(&button);
    }
    flow.show_all();
}

fn list_gifs(gif_dir: &Path) -> Vec<PathBuf> {
    let mut gifs = fs::read_dir(gif_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|ext| ext.to_str()).map(|ext| ext.eq_ignore_ascii_case("gif")).unwrap_or(false)
        })
        .collect::<Vec<_>>();
    gifs.sort_by_key(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default());
    gifs
}

fn copy_gif(path: &Path, clipboard: &gtk::Clipboard, status: &gtk::Label) -> bool {
    let result = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "image/gif", "-i"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(exit) if exit.success() => {
            status.set_text("GIF copied as image/gif — paste with Ctrl+V in a compatible app.");
            true
        }
        _ => {
            clipboard.set_text(&path.to_string_lossy());
            clipboard.store();
            status.set_text("xclip is missing; copied the GIF file path instead. Install xclip for GIF paste.");
            false
        }
    }
}

fn render_gif_flow(
    flow: &gtk::FlowBox,
    gif_dir: &Path,
    query: &str,
    clipboard: &gtk::Clipboard,
    status: &gtk::Label,
) {
    clear_flow(flow);
    let normalized = query.trim().to_lowercase();
    let gifs = list_gifs(gif_dir);
    for path in gifs.iter().filter(|path| {
        normalized.is_empty()
            || path.file_name().map(|name| name.to_string_lossy().to_lowercase().contains(&normalized)).unwrap_or(false)
    }) {
        let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "GIF".into());
        let card = gtk::Box::new(gtk::Orientation::Vertical, 5);
        card.set_margin_top(5);
        card.set_margin_bottom(5);
        card.set_margin_start(5);
        card.set_margin_end(5);

        if let Ok(pixbuf) = Pixbuf::from_file_at_scale(path, 124, 82, true) {
            let image = gtk::Image::from_pixbuf(Some(&pixbuf));
            image.set_size_request(124, 82);
            card.pack_start(&image, true, true, 0);
        } else {
            let placeholder = gtk::Label::new(Some("GIF"));
            placeholder.set_size_request(124, 82);
            card.pack_start(&placeholder, true, true, 0);
        }
        let label = gtk::Label::new(Some(&name));
        label.set_max_width_chars(18);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        card.pack_start(&label, false, false, 0);

        let button = gtk::Button::new();
        button.set_size_request(150, 122);
        button.set_tooltip_text(Some("Copy the animated GIF. Paste it with Ctrl+V."));
        button.style_context().add_class("gif-cell");
        button.add(&card);
        let gif_path = path.clone();
        let clip = clipboard.clone();
        let status_label = status.clone();
        button.connect_clicked(move |_| {
            // GTK signal handlers return (), but copy_gif reports success as bool.
            let _copied = copy_gif(&gif_path, &clip, &status_label);
        });
        flow.add(&button);
    }
    flow.show_all();
}

fn pixbuf_from_bytes(bytes: &[u8]) -> Option<Pixbuf> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(bytes).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
}

fn render_remote_gif_flow(
    flow: &gtk::FlowBox,
    gifs: &[RemoteGif],
    gif_dir: &Path,
    sender: &Sender<GifWorkerEvent>,
    status: &gtk::Label,
    customer_id: &str,
) {
    clear_flow(flow);
    for gif in gifs {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 5);
        card.set_margin_top(5);
        card.set_margin_bottom(5);
        card.set_margin_start(5);
        card.set_margin_end(5);

        let image_added = gif.preview_bytes.as_ref().and_then(|bytes| pixbuf_from_bytes(bytes));
        if let Some(pixbuf) = image_added {
            let image = gtk::Image::from_pixbuf(Some(&pixbuf));
            image.set_size_request(124, 82);
            card.pack_start(&image, true, true, 0);
        } else {
            let placeholder = gtk::Label::new(Some("GIF"));
            placeholder.set_size_request(124, 82);
            card.pack_start(&placeholder, true, true, 0);
        }

        let title = gtk::Label::new(Some(&gif.title));
        title.set_max_width_chars(20);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        card.pack_start(&title, false, false, 0);

        let button = gtk::Button::new();
        button.set_size_request(150, 122);
        button.set_tooltip_text(Some("Download and copy this animated GIF"));
        button.style_context().add_class("gif-cell");
        button.add(&card);

        let selected_gif = gif.clone();
        let selected_dir = gif_dir.to_path_buf();
        let event_sender = sender.clone();
        let status_label = status.clone();
        let selected_customer_id = customer_id.to_owned();
        button.connect_clicked(move |_| {
            status_label.set_text("Downloading GIF from GIPHY…");
            send_giphy_analytics(selected_gif.analytics_onclick.clone(), selected_customer_id.clone(), "onclick");
            let gif = selected_gif.clone();
            let dir = selected_dir.clone();
            let tx = event_sender.clone();
            let customer_for_event = selected_customer_id.clone();
            let _ = thread::Builder::new()
                .name("giphy-gif-download".into())
                .spawn(move || match download_giphy_gif(&gif, &dir) {
                    Ok(path) => {
                        let _ = tx.send(GifWorkerEvent::DownloadReady {
                            path,
                            analytics_url: gif.analytics_onsent,
                            customer_id: customer_for_event,
                        });
                    }
                    Err(message) => { let _ = tx.send(GifWorkerEvent::DownloadFailed(message)); }
                });
        });
        flow.add(&button);
    }
    flow.show_all();
    for gif in gifs {
        send_giphy_analytics(gif.analytics_onload.clone(), customer_id.to_owned(), "onload");
    }
}

fn import_gif(parent: &ApplicationWindow, gif_dir: &Path) -> Result<Option<PathBuf>, String> {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some("Add a GIF"),
        Some(parent),
        gtk::FileChooserAction::Open,
        &[("Cancel", gtk::ResponseType::Cancel), ("Add GIF", gtk::ResponseType::Accept)],
    );
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("GIF images (*.gif)"));
    filter.add_mime_type("image/gif");
    filter.add_pattern("*.gif");
    filter.add_pattern("*.GIF");
    dialog.add_filter(filter);

    let selected = if dialog.run() == gtk::ResponseType::Accept {
        dialog.filename()
    } else {
        None
    };
    // `destroy()` is unsafe in gtk-rs 0.18. Hiding and dropping the local
    // dialog reference releases it safely after the synchronous response.
    dialog.hide();

    let Some(source) = selected else { return Ok(None); };
    if source.extension().and_then(|ext| ext.to_str()).map(|ext| !ext.eq_ignore_ascii_case("gif")).unwrap_or(true) {
        return Err("Please select a .gif file.".into());
    }
    let file_name = source.file_name().ok_or_else(|| "The selected file has no name.".to_string())?;
    let mut destination = gif_dir.join(file_name);
    if destination.exists() {
        let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("gif");
        let mut counter = 2usize;
        loop {
            destination = gif_dir.join(format!("{stem}-{counter}.gif"));
            if !destination.exists() { break; }
            counter += 1;
        }
    }
    fs::copy(source, &destination).map_err(|error| format!("Could not copy GIF: {error}"))?;
    Ok(Some(destination))
}

fn prompt_for_giphy_key(parent: &ApplicationWindow, current_key: &str) -> Option<String> {
    let dialog = gtk::Dialog::with_buttons(
        Some("GIPHY GIF API key"),
        Some(parent),
        gtk::DialogFlags::MODAL,
        &[("Cancel", gtk::ResponseType::Cancel), ("Save", gtk::ResponseType::Accept)],
    );
    dialog.set_default_size(470, 150);

    let content = dialog.content_area();
    let explanation = gtk::Label::new(Some(
        "Create an API key in the GIPHY Developer Dashboard, then paste it here. The key is saved locally with user-only file permissions. GIPHY beta keys are rate-limited.",
    ));
    explanation.set_line_wrap(true);
    explanation.set_xalign(0.0);
    explanation.set_margin_start(12);
    explanation.set_margin_end(12);
    explanation.set_margin_top(12);
    content.pack_start(&explanation, false, false, 8);

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Paste your GIPHY API key"));
    entry.set_visibility(false);
    entry.set_text(current_key);
    entry.set_margin_start(12);
    entry.set_margin_end(12);
    content.pack_start(&entry, false, false, 8);

    let help = gtk::Label::new(Some("https://developers.giphy.com/dashboard/"));
    help.set_xalign(0.0);
    help.set_margin_start(12);
    help.set_margin_end(12);
    content.pack_start(&help, false, false, 4);

    dialog.show_all();
    let response = dialog.run();
    let key = if response == gtk::ResponseType::Accept {
        Some(entry.text().trim().to_owned())
    } else {
        None
    };
    // `destroy()` is unsafe in gtk-rs 0.18. Hiding and dropping the local
    // dialog reference releases it safely after the synchronous response.
    dialog.hide();
    key
}

fn apply_css(provider: &gtk::CssProvider, dark_mode: bool) {
    // Keep one provider attached to the screen and reload it when the user
    // switches themes. This avoids accumulating CSS providers on every toggle.
    let css = if dark_mode {
        r#"
            window, dialog, messagedialog, filechooser { background-color: #171b22; color: #e7edf5; }
            .app-shell { background-color: #171b22; color: #e7edf5; border: 1px solid #303b49; border-radius: 16px; }
            .topbar { background-color: #202833; border-bottom: 1px solid #354252; padding: 12px 16px; }
            .app-title { font-size: 17px; font-weight: 700; color: #f2f6fb; }
            .subtitle { color: #a4b1c2; font-size: 10px; }
            label { color: #e0e7f0; }
            .muted { color: #a0adbf; font-size: 10px; }
            .search-entry, entry { background-color: #252f3c; border: 1px solid #405064; border-radius: 10px; padding: 10px 12px; color: #f1f5f9; caret-color: #8cc8ff; }
            entry selection { background-color: #315d83; color: #ffffff; }
            notebook, notebook stack, scrolledwindow, viewport, flowbox { background-color: #171b22; color: #e7edf5; }
            notebook header { background-color: #202833; border: 0; }
            notebook tab { background-color: transparent; padding: 10px 18px; color: #b1bdcc; }
            notebook tab:hover { background-color: #293646; color: #f2f6fb; }
            notebook tab:checked { background-color: #263748; color: #8cc8ff; border-bottom: 2px solid #56a8ef; }
            button { background-image: none; background-color: #26313e; border: 1px solid #3b495a; color: #e7edf5; }
            button:hover { background-color: #334458; border-color: #6383a5; color: #ffffff; }
            button:active, button:checked { background-color: #304b65; }
            button:disabled { background-color: #202833; color: #788596; border-color: #303b49; }
            .emoji-cell { background-color: #252f3c; border: 1px solid #354354; border-radius: 10px; color: #f1f5f9; font-size: 22px; padding: 5px; }
            .art-cell { background-color: #252f3c; border: 1px solid #354354; border-radius: 10px; font-size: 17px; padding: 7px; color: #e7edf5; }
            .gif-cell { background-color: #252f3c; border: 1px solid #354354; border-radius: 12px; padding: 6px; color: #e7edf5; }
            .history-cell { background-color: #252f3c; border: 1px solid #354354; border-radius: 8px; padding: 9px; color: #e7edf5; }
            .emoji-cell:hover, .art-cell:hover, .gif-cell:hover, .history-cell:hover { background-color: #30475e; border-color: #6396c2; }
            .giphy-attribution { color: #8cc8ff; font-size: 12px; font-weight: 700; padding-top: 3px; }
            .statusbar { background-color: #202833; border-top: 1px solid #354252; padding: 7px 12px; color: #b1bdcc; font-size: 10px; }
            button:focus, entry:focus { border-color: #69b7f5; }
            .primary-button, .theme-button { background-image: none; background-color: #29445d; color: #c5e4ff; border: 1px solid #426889; border-radius: 8px; padding: 7px 12px; }
            .primary-button:hover, .theme-button:hover { background-color: #355a78; }
            .clear-button { background-image: none; background-color: #4a292b; color: #ffc4c0; border: 1px solid #704044; border-radius: 8px; padding: 7px 12px; }
            .clear-button:hover { background-color: #603235; }
            scrollbar trough { background-color: #1d2530; }
            scrollbar slider { background-color: #536174; border-radius: 8px; }
            tooltip { background-color: #293544; color: #f1f5f9; border: 1px solid #43536a; }
            separator { background-color: #354252; }
        "#
    } else {
        r#"
            window, dialog, messagedialog, filechooser { background: #f4f7fc; color: #1f2937; }
            .app-shell { background: #f4f7fc; border: 1px solid #dce5f1; border-radius: 16px; }
            .topbar { background: #eaf1fb; border-bottom: 1px solid #dbe5f2; padding: 12px 16px; }
            .app-title { font-size: 17px; font-weight: 700; color: #172b4d; }
            .subtitle { color: #60738f; font-size: 10px; }
            label { color: #24364d; }
            .search-entry, entry { background: #ffffff; border: 1px solid #d6e0ed; border-radius: 10px; padding: 10px 12px; color: #1f2937; }
            notebook, notebook stack, scrolledwindow, viewport, flowbox { background: #f4f7fc; }
            notebook header { background: #edf2f9; border: 0; }
            notebook tab { padding: 10px 18px; color: #5e7089; }
            notebook tab:checked { color: #0969b5; border-bottom: 2px solid #1687d9; }
            .emoji-cell { background: #ffffff; border: 1px solid #e4eaf2; border-radius: 10px; font-size: 22px; padding: 5px; }
            .emoji-cell:hover, .art-cell:hover, .gif-cell:hover { background: #e2f1ff; border-color: #8fc7f5; }
            .art-cell { background: #ffffff; border: 1px solid #e4eaf2; border-radius: 10px; font-size: 17px; padding: 7px; color: #24364d; }
            .gif-cell { background: #ffffff; border: 1px solid #e4eaf2; border-radius: 12px; padding: 6px; }
            .history-cell { background: #ffffff; border: 1px solid #e4eaf2; border-radius: 8px; padding: 9px; color: #24364d; }
            .muted { color: #718096; font-size: 10px; }
            .giphy-attribution { color: #0969b5; font-size: 12px; font-weight: 700; padding-top: 3px; }
            .statusbar { background: #edf2f9; border-top: 1px solid #dbe5f2; padding: 7px 12px; color: #52657e; font-size: 10px; }
            button { outline: none; }
            button:focus { border-color: #1687d9; }
            .primary-button, .theme-button { background: #dceeff; color: #075b9c; border: 1px solid #b6dafa; border-radius: 8px; padding: 7px 12px; }
            .primary-button:hover, .theme-button:hover { background: #c9e5fc; }
            .clear-button { background: #fff1f0; color: #a3342b; border: 1px solid #f3ceca; border-radius: 8px; padding: 7px 12px; }
        "#
    };

    if let Err(error) = provider.load_from_data(css.as_bytes()) {
        eprintln!("Could not load application theme: {error}");
    }
}

fn add_section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.style_context().add_class("muted");
    label
}

fn open_or_toggle(window: &ApplicationWindow, search: &gtk::Entry, target: &Rc<RefCell<Option<String>>>) {
    if window.is_visible() {
        window.hide();
    } else {
        // Remember the app that had focus before the picker appears.
        *target.borrow_mut() = capture_active_window();
        window.show_all();
        window.present();
        search.grab_focus();
        search.select_region(0, -1);
    }
}

fn build_ui(
    app: &Application,
    paths: &AppPaths,
    initial_visible: bool,
    receiver: Receiver<AppCommand>,
) {
    let config = Rc::new(RefCell::new(load_config(&paths.config_file)));
    let dark_mode = Rc::new(RefCell::new(config.borrow().dark_mode));
    let theme_provider = gtk::CssProvider::new();
    apply_css(&theme_provider, *dark_mode.borrow());
    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &theme_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let clipboard = gtk::Clipboard::get(&gdk::SELECTION_CLIPBOARD);
    let window = ApplicationWindow::new(app);
    window.set_title("Emoji & Clipboard");
    window.set_default_size(760, 570);
    window.set_position(gtk::WindowPosition::Center);
    window.set_resizable(true);
    window.set_keep_above(true);
    window.set_skip_taskbar_hint(true);
    window.set_icon_name(Some("face-smile-symbolic"));

    let shell = gtk::Box::new(gtk::Orientation::Vertical, 0);
    shell.style_context().add_class("app-shell");

    let topbar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    topbar.style_context().add_class("topbar");
    let brand = gtk::Box::new(gtk::Orientation::Vertical, 1);
    let title = gtk::Label::new(Some("🙂  Emoji & Clipboard"));
    title.set_xalign(0.0);
    title.style_context().add_class("app-title");
    let subtitle = gtk::Label::new(Some("A little Windows-style picker for XFCE"));
    subtitle.set_xalign(0.0);
    subtitle.style_context().add_class("subtitle");
    brand.pack_start(&title, false, false, 0);
    brand.pack_start(&subtitle, false, false, 0);
    topbar.pack_start(&brand, true, true, 0);
    let close_button = gtk::Button::with_label("✕");
    close_button.set_tooltip_text(Some("Hide picker (Esc)"));
    close_button.set_relief(gtk::ReliefStyle::None);
    topbar.pack_end(&close_button, false, false, 0);

    let theme_button = gtk::Button::new();
    theme_button.style_context().add_class("theme-button");
    theme_button.set_tooltip_text(Some("Switch between light and dark appearance"));
    theme_button.set_label(if *dark_mode.borrow() { "☀ Light" } else { "☾ Dark" });
    topbar.pack_end(&theme_button, false, false, 0);
    shell.pack_start(&topbar, false, false, 0);

    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    search_row.set_margin_top(12);
    search_row.set_margin_bottom(8);
    search_row.set_margin_start(14);
    search_row.set_margin_end(14);
    let search = gtk::Entry::new();
    search.set_placeholder_text(Some("Search emoji, kaomoji or clipboard…"));
    search.set_hexpand(true);
    search.style_context().add_class("search-entry");
    search_row.pack_start(&search, true, true, 0);
    shell.pack_start(&search_row, false, false, 0);

    let notebook = gtk::Notebook::new();
    notebook.set_scrollable(true);

    // Emoji tab
    let emoji_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    emoji_page.set_margin_start(14);
    emoji_page.set_margin_end(14);
    emoji_page.set_margin_bottom(8);
    let emoji_info = add_section_label("Loading Unicode emoji…");
    emoji_page.pack_start(&emoji_info, false, false, 2);
    let emoji_scroller = make_scroller();
    let emoji_flow = make_flow(9, true);
    emoji_scroller.add(&emoji_flow);
    emoji_page.pack_start(&emoji_scroller, true, true, 0);
    notebook.append_page(&emoji_page, Some(&gtk::Label::new(Some("Emoji"))));

    // Kaomoji / text-art tab
    let art_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    art_page.set_margin_start(14);
    art_page.set_margin_end(14);
    art_page.set_margin_bottom(8);
    art_page.pack_start(&add_section_label("Classic emoticons, kaomoji and text art — click one to insert it."), false, false, 2);
    let art_scroller = make_scroller();
    let art_flow = make_flow(3, true);
    art_scroller.add(&art_flow);
    art_page.pack_start(&art_scroller, true, true, 0);
    notebook.append_page(&art_page, Some(&gtk::Label::new(Some("Kaomoji & Art"))));

    // GIF tab: online GIPHY search plus a local GIF collection.
    let gif_page = gtk::Box::new(gtk::Orientation::Vertical, 7);
    gif_page.set_margin_start(14);
    gif_page.set_margin_end(14);
    gif_page.set_margin_bottom(8);

    let giphy_search_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let giphy_search = gtk::Entry::new();
    giphy_search.set_placeholder_text(Some("Search GIPHY GIFs… e.g. happy, cat, hello"));
    giphy_search.set_hexpand(true);
    giphy_search_row.pack_start(&giphy_search, true, true, 0);
    let giphy_search_button = gtk::Button::with_label("Search GIPHY");
    giphy_search_button.style_context().add_class("primary-button");
    giphy_search_row.pack_start(&giphy_search_button, false, false, 0);
    let giphy_key_button = gtk::Button::with_label("API Key");
    giphy_key_button.style_context().add_class("primary-button");
    giphy_search_row.pack_start(&giphy_key_button, false, false, 0);
    gif_page.pack_start(&giphy_search_row, false, false, 2);

    let gif_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    gif_actions.pack_start(&add_section_label("Search GIPHY or use your own animated GIFs"), true, true, 0);
    let my_gifs_button = gtk::Button::with_label("My GIFs");
    my_gifs_button.style_context().add_class("primary-button");
    let add_gif_button = gtk::Button::with_label("＋ Add GIF");
    add_gif_button.style_context().add_class("primary-button");
    let open_folder_button = gtk::Button::with_label("Open folder");
    open_folder_button.style_context().add_class("primary-button");
    gif_actions.pack_end(&open_folder_button, false, false, 0);
    gif_actions.pack_end(&add_gif_button, false, false, 0);
    gif_actions.pack_end(&my_gifs_button, false, false, 0);
    gif_page.pack_start(&gif_actions, false, false, 2);

    let gif_scroller = make_scroller();
    let gif_flow = make_flow(4, true);
    gif_scroller.add(&gif_flow);
    gif_page.pack_start(&gif_scroller, true, true, 0);
    let giphy_attribution = gtk::Label::new(Some("Powered By GIPHY"));
    giphy_attribution.set_xalign(0.0);
    giphy_attribution.style_context().add_class("giphy-attribution");
    gif_page.pack_start(&giphy_attribution, false, false, 0);
    let gif_note = add_section_label("Click a result to download and copy the animated GIF. Some apps may not accept GIF clipboard data.");
    gif_page.pack_start(&gif_note, false, false, 0);
    notebook.append_page(&gif_page, Some(&gtk::Label::new(Some("GIFs"))));

    // Clipboard history tab
    let history_page = gtk::Box::new(gtk::Orientation::Vertical, 7);
    history_page.set_margin_start(14);
    history_page.set_margin_end(14);
    history_page.set_margin_bottom(8);
    let history_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    history_actions.pack_start(&add_section_label("Recent text copied on this computer"), true, true, 0);
    let clear_history_button = gtk::Button::with_label("Clear history");
    clear_history_button.style_context().add_class("clear-button");
    history_actions.pack_end(&clear_history_button, false, false, 0);
    history_page.pack_start(&history_actions, false, false, 2);
    let history_scroller = make_scroller();
    let history_flow = make_flow(1, false);
    history_scroller.add(&history_flow);
    history_page.pack_start(&history_scroller, true, true, 0);
    let privacy_note = add_section_label("History is stored locally in ~/.local/share/emoji-clipboard/history.json. Clear it any time.");
    history_page.pack_start(&privacy_note, false, false, 2);
    notebook.append_page(&history_page, Some(&gtk::Label::new(Some("Clipboard"))));

    shell.pack_start(&notebook, true, true, 0);
    let status = gtk::Label::new(Some("Ready  •  Win+; opens this picker  •  Esc hides it"));
    status.set_xalign(0.0);
    status.style_context().add_class("statusbar");
    shell.pack_end(&status, false, false, 0);
    window.add(&shell);

    let previous_window = Rc::new(RefCell::new(None::<String>));
    let choose: Rc<dyn Fn(String)> = {
        let clip = clipboard.clone();
        let win = window.clone();
        let target = previous_window.clone();
        Rc::new(move |text| choose_text(text, &clip, &win, &target))
    };

    let emoji_data = Rc::new(build_emoji_data());
    let kaomoji_data = Rc::new(build_kaomoji_data());
    let history = Rc::new(RefCell::new(load_history(&paths.history_file)));
    let last_clipboard = Rc::new(RefCell::new(String::new()));

    let render_all = {
        let emoji_flow = emoji_flow.clone();
        let emoji_info = emoji_info.clone();
        let art_flow = art_flow.clone();
        let history_flow = history_flow.clone();
        let emoji_items = emoji_data.clone();
        let art_items = kaomoji_data.clone();
        let stored_history = history.clone();
        let callback = choose.clone();
        Rc::new(move |query: &str| {
            render_emoji_flow(&emoji_flow, &emoji_info, &emoji_items, query, &callback);
            render_kaomoji_flow(&art_flow, &art_items, query, &callback);
            render_history_flow(&history_flow, &stored_history.borrow(), query, &callback);
        })
    };

    render_all("");
    render_gif_flow(&gif_flow, &paths.gif_dir, "", &clipboard, &status);
    {
        let render = render_all.clone();
        search.connect_changed(move |entry| render(&entry.text().to_string()));
    }

    {
        let win = window.clone();
        close_button.connect_clicked(move |_| win.hide());
    }
    {
        let win = window.clone();
        let gif_dir = paths.gif_dir.clone();
        let gif_grid = gif_flow.clone();
        let clip = clipboard.clone();
        let status_label = status.clone();
        add_gif_button.connect_clicked(move |_| match import_gif(&win, &gif_dir) {
            Ok(Some(path)) => {
                let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("GIF");
                status_label.set_text(&format!("Added {name}"));
                render_gif_flow(&gif_grid, &gif_dir, "", &clip, &status_label);
            }
            Ok(None) => {}
            Err(error) => status_label.set_text(&error),
        });
    }
    {
        let gif_dir = paths.gif_dir.clone();
        open_folder_button.connect_clicked(move |_| {
            let _ = Command::new("xdg-open").arg(&gif_dir).spawn();
        });
    }
    {
        let stored_history = history.clone();
        let history_file = paths.history_file.clone();
        let search_entry = search.clone();
        let render = render_all.clone();
        let status_label = status.clone();
        clear_history_button.connect_clicked(move |_| {
            stored_history.borrow_mut().clear();
            save_history(&history_file, &stored_history.borrow());
            render(&search_entry.text().to_string());
            status_label.set_text("Clipboard history cleared.");
        });
    }

    // Persistent light/dark switch. Config serde defaults keep older installs compatible.
    {
        let theme_state = dark_mode.clone();
        let provider = theme_provider.clone();
        let app_config = config.clone();
        let config_file = paths.config_file.clone();
        let button = theme_button.clone();
        let status_label = status.clone();
        theme_button.connect_clicked(move |_| {
            let next_dark = !*theme_state.borrow();
            *theme_state.borrow_mut() = next_dark;
            apply_css(&provider, next_dark);
            button.set_label(if next_dark { "☀ Light" } else { "☾ Dark" });

            let mut updated = app_config.borrow().clone();
            updated.dark_mode = next_dark;
            match save_config(&config_file, &updated) {
                Ok(()) => {
                    *app_config.borrow_mut() = updated;
                    status_label.set_text(if next_dark {
                        "Dark mode enabled. Your appearance preference was saved."
                    } else {
                        "Light mode enabled. Your appearance preference was saved."
                    });
                }
                Err(error) => status_label.set_text(&format!("Theme changed, but could not save it: {error}")),
            }
        });
    }

    {
        let win = window.clone();
        window.connect_delete_event(move |_, _| {
            win.hide();
            gtk::glib::Propagation::Stop
        });
    }
    {
        let win = window.clone();
        window.connect_key_press_event(move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                win.hide();
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
    }

    let toggle = {
        let win = window.clone();
        let search_entry = search.clone();
        let target = previous_window.clone();
        Rc::new(move || open_or_toggle(&win, &search_entry, &target))
    };

    // GIPHY API key and appearance preference are stored in the local app config.
    let (gif_event_tx, gif_event_rx) = mpsc::channel::<GifWorkerEvent>();
    let active_gif_query = Rc::new(RefCell::new(String::new()));

    let configure_key = {
        let parent = window.clone();
        let app_config = config.clone();
        let config_file = paths.config_file.clone();
        let status_label = status.clone();
        Rc::new(move || {
            let mut draft = app_config.borrow().clone();
            if let Some(key) = prompt_for_giphy_key(&parent, &draft.giphy_api_key) {
                draft.giphy_api_key = key;
                match save_config(&config_file, &draft) {
                    Ok(()) => {
                        *app_config.borrow_mut() = draft;
                        status_label.set_text("GIPHY API key saved securely.");
                    }
                    Err(error) => status_label.set_text(&error),
                }
            }
        })
    };
    {
        let configure = configure_key.clone();
        giphy_key_button.connect_clicked(move |_| configure());
    }

    let launch_giphy_search = {
        let api_config = config.clone();
        let event_sender = gif_event_tx.clone();
        let query_state = active_gif_query.clone();
        let status_label = status.clone();
        Rc::new(move |query: String| {
            let query = query.trim().to_owned();
            if query.is_empty() {
                status_label.set_text("Type a word or phrase to search GIPHY.");
                return;
            }
            *query_state.borrow_mut() = query.clone();
            let (api_key, existing_customer_id) = {
                let cfg = api_config.borrow();
                (cfg.giphy_api_key.clone(), cfg.giphy_customer_id.clone())
            };
            let sender = event_sender.clone();
            status_label.set_text("Searching GIPHY…");
            let worker_query = query.clone();
            let _ = thread::Builder::new()
                .name("giphy-search".into())
                .spawn(move || match search_giphy(&worker_query, &api_key, &existing_customer_id) {
                    Ok((gifs, customer_id)) => {
                        let _ = sender.send(GifWorkerEvent::SearchReady { query: worker_query, gifs, customer_id });
                    }
                    Err(message) => { let _ = sender.send(GifWorkerEvent::SearchFailed { query: worker_query, message }); }
                });
        })
    };
    {
        let search_entry = giphy_search.clone();
        let launch = launch_giphy_search.clone();
        giphy_search_button.connect_clicked(move |_| launch(search_entry.text().to_string()));
    }
    {
        let launch = launch_giphy_search.clone();
        giphy_search.connect_activate(move |entry| launch(entry.text().to_string()));
    }

    {
        let grid = gif_flow.clone();
        let gif_dir = paths.gif_dir.clone();
        let clip = clipboard.clone();
        let status_label = status.clone();
        my_gifs_button.connect_clicked(move |_| {
            render_gif_flow(&grid, &gif_dir, "", &clip, &status_label);
            status_label.set_text("Showing your local GIF collection.");
        });
    }

    // Apply search/download events on the GTK main thread; network work never blocks the UI.
    {
        let events = Rc::new(RefCell::new(gif_event_rx));
        let grid = gif_flow.clone();
        let gif_dir = paths.gif_dir.clone();
        let event_sender = gif_event_tx.clone();
        let status_label = status.clone();
        let query_state = active_gif_query.clone();
        let clip = clipboard.clone();
        let app_config = config.clone();
        let config_file = paths.config_file.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            loop {
                let event = match events.borrow_mut().try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return gtk::glib::ControlFlow::Break,
                };
                match event {
                    GifWorkerEvent::SearchReady { query, gifs, customer_id } => {
                        if *query_state.borrow() == query {
                            let mut updated_config = app_config.borrow().clone();
                            if updated_config.giphy_customer_id != customer_id {
                                updated_config.giphy_customer_id = customer_id.clone();
                                if save_config(&config_file, &updated_config).is_ok() {
                                    *app_config.borrow_mut() = updated_config;
                                }
                            }
                            if gifs.is_empty() {
                                clear_flow(&grid);
                                status_label.set_text("No GIFs found. Try another search.");
                            } else {
                                render_remote_gif_flow(&grid, &gifs, &gif_dir, &event_sender, &status_label, &customer_id);
                                status_label.set_text(&format!("Found {} GIFs on GIPHY. Click one to download and copy.", gifs.len()));
                            }
                        }
                    }
                    GifWorkerEvent::SearchFailed { query, message } => {
                        if *query_state.borrow() == query {
                            status_label.set_text(&message);
                        }
                    }
                    GifWorkerEvent::DownloadReady { path, analytics_url, customer_id } => {
                        if copy_gif(&path, &clip, &status_label) {
                            send_giphy_analytics(analytics_url, customer_id, "onsent");
                        }
                    }
                    GifWorkerEvent::DownloadFailed(message) => status_label.set_text(&message),
                }
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    // Toggle commands from the XFCE shortcut arrive through the small local socket.
    let toggle_for_ipc = toggle.clone();
    let app_for_ipc = app.clone();
    let receiver = Rc::new(RefCell::new(receiver));
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        loop {
            match receiver.borrow().try_recv() {
                Ok(AppCommand::Toggle) => toggle_for_ipc(),
                Ok(AppCommand::Quit) => app_for_ipc.quit(),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return gtk::glib::ControlFlow::Break,
            }
        }
        gtk::glib::ControlFlow::Continue
    });

    // A light clipboard poller avoids a heavy clipboard-monitoring dependency.
    {
        let clip = clipboard.clone();
        let history_data = history.clone();
        let history_file = paths.history_file.clone();
        let last_seen = last_clipboard.clone();
        let history_flow = history_flow.clone();
        let choose = choose.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(900), move || {
            if let Some(text) = clip.wait_for_text() {
                let text = text.to_string();
                if *last_seen.borrow() != text {
                    *last_seen.borrow_mut() = text.clone();
                    if add_history(&mut history_data.borrow_mut(), &text) {
                        save_history(&history_file, &history_data.borrow());
                        if history_flow.is_visible() {
                            render_history_flow(&history_flow, &history_data.borrow(), "", &choose);
                        }
                    }
                }
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    if initial_visible {
        open_or_toggle(&window, &search, &previous_window);
    }
}

fn print_help() {
    println!("Emoji & Clipboard for XFCE\n\nUsage:\n  emoji-clipboard             Open the picker (or start it)\n  emoji-clipboard --background  Start hidden in the background\n  emoji-clipboard --toggle      Toggle the running picker\n  emoji-clipboard --quit        Quit the running instance\n  emoji-clipboard --help        Show this help\n\nDefault XFCE shortcut: Super+semicolon (Win+;)");
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("emoji-clipboard {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let paths = app_paths();
    if let Err(error) = ensure_app_dirs(&paths) {
        eprintln!("Could not create the Emoji Clipboard data folder: {error}");
        std::process::exit(1);
    }

    let requested_toggle = args.iter().any(|arg| arg == "--toggle");
    let requested_quit = args.iter().any(|arg| arg == "--quit");
    let background = args.iter().any(|arg| arg == "--background");

    if requested_toggle {
        if send_command("toggle", &paths.socket_file) {
            return;
        }
    } else if requested_quit {
        let _ = send_command("quit", &paths.socket_file);
        return;
    } else if background && UnixStream::connect(&paths.socket_file).is_ok() {
        return;
    }

    let (sender, receiver) = mpsc::channel();
    if let Err(error) = start_command_server(&paths.socket_file, sender) {
        eprintln!("Could not start the local command socket: {error}");
        std::process::exit(1);
    }

    let app = Application::new(Some(APP_ID), gtk::gio::ApplicationFlags::NON_UNIQUE);
    let initial_visible = !background;
    let paths_for_activate = paths;
    let socket_file = paths_for_activate.socket_file.clone();
    let receiver_once = Rc::new(RefCell::new(Some(receiver)));
    app.connect_activate({
        let paths_for_activate = paths_for_activate;
        let receiver_once = receiver_once.clone();
        move |app| {
            if let Some(receiver) = receiver_once.borrow_mut().take() {
                build_ui(app, &paths_for_activate, initial_visible, receiver);
            }
        }
    });
    app.connect_shutdown(move |_| {
        let _ = fs::remove_file(&socket_file);
    });
    // We consume --background/--toggle/--quit ourselves above. gtk-rs 0.18's
    // ApplicationExtManual::run() forwards the *original process arguments*
    // to GApplication, which then rejects our private flags as unknown options.
    // Pass only argv[0] so GTK activates the app without reparsing our CLI flags.
    let program_name = env::args()
        .next()
        .unwrap_or_else(|| "emoji-clipboard".to_string());
    let _ = app.run_with_args(&[program_name]);
}
