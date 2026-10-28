//! Discord rich presence sent straight to Discord's gateway as the user, so the Discord app
//! does not need to be running. The activity is named after the artist and speed, never the player.

use std::{cell::RefCell, collections::HashMap, io::{ErrorKind, Write}, process::{Command, Stdio}, sync::{Arc, Mutex, mpsc}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

use serde_json::{Value, json};
use tungstenite::{Message, protocol::{CloseFrame, frame::coding::CloseCode}, stream::MaybeTlsStream};

const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
/// Stay connected this long after playback stops, so a short pause does not flicker your status.
const LINGER: Duration = Duration::from_secs(300);
const LONGEST_BACKOFF: Duration = Duration::from_secs(60);
/// A Discord application, needed only so Discord displays album art. Its name is never shown.
const APPLICATION_ID: &str = "1559622888431030282";

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub title: String,
    pub artist: Option<String>,
    /// Wall-clock milliseconds when the song would have started and will end at the current speed.
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    /// Public image URLs, best first. Discord fetches art through its media proxy,
    /// so it is given the first one that loads.
    pub art_urls: Vec<String>,
    pub speed: f64,
}

impl Activity {
    pub fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
    }

    /// Whether Discord already shows this, ignoring the timeline jitter of polling the player.
    fn same(&self, other: &Self) -> bool {
        let close = |a: i64, b: i64| (a - b).abs() < 2000;
        self.title == other.title && self.artist == other.artist && self.speed == other.speed && self.art_urls == other.art_urls && close(self.start_ms, other.start_ms)
            && match (self.end_ms, other.end_ms) { (Some(a), Some(b)) => close(a, b), (a, b) => a == b }
    }
}

/// Discord rejects activity text longer than 128 characters.
fn clip(text: &str) -> String { text.chars().take(128).collect() }

/// `art` is the image's media proxy path, from `proxy_path`. `status` is the user's status as Discord reports it,
/// so this never overrides Invisible or Do Not Disturb set elsewhere. The session is marked away, so Discord
/// still sends push notifications to the user's phone.
fn presence(activity: Option<&Activity>, art: Option<&str>, status: &str, since: i64) -> Value {
    let activities: Vec<Value> = activity.into_iter().map(|activity| {
        // The profile shows "Listening to" the name, while status_display_type picks what the member list
        // shows after the music note: the artist (state), which is more recognizable than the song title,
        // as in Discord's Spotify presence, or the title (details) when there is no artist.
        let speed = format!("{:.2}", activity.speed);
        let speed = speed.trim_end_matches('0').trim_end_matches('.');
        let mut value = json!({
            "name": clip(&format!("{} ({speed}× speed)", activity.artist.as_deref().unwrap_or(&activity.title))),
            "type": 2, "details": clip(&activity.title), "status_display_type": if activity.artist.is_some() { 1 } else { 2 },
            "timestamps": {"start": activity.start_ms}, "application_id": APPLICATION_ID,
        });
        if let Some(art) = art { value["assets"] = json!({"large_image": format!("mp:{art}")}); }
        if let Some(artist) = &activity.artist { value["state"] = clip(artist).into(); }
        if let Some(end) = activity.end_ms { value["timestamps"]["end"] = end.into(); }
        value
    }).collect();
    json!({"since": since, "afk": true, "status": status, "activities": activities})
}

/// The user's overall status from a READY or SESSIONS_REPLACE session list, if Discord included it.
fn discord_status(sessions: &Value) -> Option<String> {
    sessions.as_array()?.iter().find(|session| session["session_id"] == "all")?["status"].as_str()
        .filter(|status| !status.is_empty() && *status != "unknown").map(str::to_owned)
}

/// What is known about album art, shared with the threads that look it up.
#[derive(Default)]
struct ArtCache {
    /// Media proxy paths of images Discord has already registered.
    paths: HashMap<String, String>,
    loads: HashMap<String, bool>,
}

/// Whether `url` loads. Discord shows a broken image for a missing one, such as a YouTube video without a
/// high-resolution thumbnail. Only real answers are remembered, so a network hiccup does not hide the art
/// until restart; servers that refuse HEAD requests usually still serve the image.
fn loads(cache: &Mutex<ArtCache>, url: &str) -> bool {
    if let Some(&loads) = cache.lock().unwrap().loads.get(url) { return loads; }
    let Ok(response) = reqwest::blocking::Client::new().head(url).timeout(Duration::from_secs(10)).send() else { return false };
    let loads = response.status().is_success() || response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED;
    cache.lock().unwrap().loads.insert(url.to_owned(), loads);
    loads
}

fn proxy_path(cache: &Mutex<ArtCache>, token: &str, url: &str) -> Option<String> {
    if let Some(path) = cache.lock().unwrap().paths.get(url) { return Some(path.clone()); }
    let response = reqwest::blocking::Client::new()
        .post(format!("https://discord.com/api/v9/applications/{APPLICATION_ID}/external-assets"))
        .header("Authorization", token).json(&json!({"urls": [url]})).timeout(Duration::from_secs(10)).send()
        .and_then(|response| response.error_for_status()).and_then(|response| response.json::<Value>());
    let path = match response {
        Ok(value) => value[0]["external_asset_path"].as_str()?.to_owned(),
        Err(error) => { eprintln!("Discord could not load album art: {error}"); return None; }
    };
    cache.lock().unwrap().paths.insert(url.to_owned(), path.clone());
    Some(path)
}

enum Request { Token(Option<String>), Activity(Option<Activity>), Art(Vec<String>, Option<String>), Quit }

/// What the gateway thread shows, kept across reconnects.
struct State {
    token: Option<String>,
    activity: Option<Activity>,
    /// The art URLs last looked up, and the media proxy path found for them. Art is looked up on another
    /// thread, so slow image hosts never delay heartbeats; meanwhile updates wait, so a new song is not
    /// first shown without art.
    art_for: Option<Vec<String>>,
    art: Option<String>,
    art_pending: bool,
    cache: Arc<Mutex<ArtCache>>,
    requests: mpsc::Sender<Request>,
}

impl State {
    /// Returns false when the thread should stop.
    fn apply(&mut self, request: Request) -> bool {
        match request {
            Request::Token(new) => {
                self.token = new.filter(|new| !new.is_empty());
                // Art is registered with the token, so look it up again.
                self.art_for = None;
                self.look_up_art();
            }
            Request::Activity(new) => { self.activity = new; self.look_up_art(); }
            Request::Art(urls, path) => if self.art_for.as_ref() == Some(&urls) { self.art = path; self.art_pending = false; },
            Request::Quit => return false,
        }
        true
    }

    fn look_up_art(&mut self) {
        let (Some(token), Some(activity)) = (&self.token, &self.activity) else { return };
        if self.art_for.as_ref() == Some(&activity.art_urls) { return; }
        let urls = activity.art_urls.clone();
        self.art_for = Some(urls.clone());
        self.art = None;
        self.art_pending = false;
        if urls.is_empty() { return; }
        let (token, cache, requests) = (token.clone(), self.cache.clone(), self.requests.clone());
        self.art_pending = std::thread::Builder::new().name("discord art".into()).spawn(move || {
            let path = urls.iter().find(|url| loads(&cache, url)).and_then(|url| proxy_path(&cache, &token, url));
            let _ = requests.send(Request::Art(urls, path));
        }).is_ok();
    }

    fn update(&self, status: &str, since: i64) -> Value {
        json!({"op": 3, "d": presence(self.activity.as_ref(), self.art.as_deref(), status, since)})
    }
}

/// Keeps a gateway session on a background thread while there is a token and a song playing.
pub struct Presence {
    requests: mpsc::Sender<Request>,
    last: RefCell<Option<Activity>>,
    /// Disconnects when the thread exits.
    finished: mpsc::Receiver<()>,
}

impl Presence {
    /// `errors` receives problems the user must fix, such as a rejected token.
    pub fn new(token: Option<String>, errors: async_channel::Sender<String>) -> Self {
        let (requests, receiver) = mpsc::channel();
        let _ = requests.send(Request::Token(token));
        let (running, finished) = mpsc::channel::<()>();
        let sender = requests.clone();
        std::thread::Builder::new().name("discord".into()).spawn(move || { let _running = running; run(receiver, sender, errors) })
            .expect("Could not start the Discord presence thread");
        Self { requests, last: RefCell::new(None), finished }
    }

    pub fn set_token(&self, token: Option<String>) { let _ = self.requests.send(Request::Token(token)); }

    pub fn update(&self, activity: Option<Activity>) {
        let changed = match (&*self.last.borrow(), &activity) {
            (Some(last), Some(activity)) => !last.same(activity),
            (last, activity) => last.is_some() != activity.is_some(),
        };
        if changed {
            *self.last.borrow_mut() = activity.clone();
            let _ = self.requests.send(Request::Activity(activity));
        }
    }
}

impl Drop for Presence {
    /// Signs out before the app exits. Otherwise Discord keeps showing the song until the session times out.
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Quit);
        let _ = self.finished.recv_timeout(Duration::from_secs(2));
    }
}

enum Ended { Quit, Idle, TokenChanged, Rejected, Failed(String) }

fn run(requests: mpsc::Receiver<Request>, sender: mpsc::Sender<Request>, errors: async_channel::Sender<String>) {
    let mut state = State { token: None, activity: None, art_for: None, art: None, art_pending: false, cache: Arc::default(), requests: sender };
    let mut backoff = Duration::from_secs(1);
    loop {
        while state.token.is_none() || state.activity.is_none() {
            match requests.recv() {
                Ok(request) => if !state.apply(request) { return; },
                Err(_) => return,
            }
        }
        match session(&requests, &mut state) {
            Ended::Quit => return,
            Ended::Idle | Ended::TokenChanged => backoff = Duration::from_secs(1),
            Ended::Rejected => {
                state.token = None;
                let _ = errors.send_blocking("Discord rejected the token. Enter a new one in Preferences.".into());
            }
            Ended::Failed(error) => {
                eprintln!("Discord presence disconnected: {error}");
                let deadline = Instant::now() + backoff;
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    match requests.recv_timeout(left) {
                        Ok(request) => if !state.apply(request) { return; },
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                backoff = (backoff * 2).min(LONGEST_BACKOFF);
            }
        }
    }
}

fn session(requests: &mpsc::Receiver<Request>, state: &mut State) -> Ended {
    let current = state.token.clone().unwrap_or_default();
    let mut socket = match tungstenite::connect(GATEWAY) { Ok((socket, _)) => socket, Err(error) => return Ended::Failed(error.to_string()) };
    // Wake regularly to heartbeat and pick up new songs.
    let timeout = Some(Duration::from_millis(250));
    let _ = match socket.get_ref() {
        MaybeTlsStream::Rustls(stream) => stream.get_ref().set_read_timeout(timeout),
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        _ => Ok(()),
    };
    let send = |socket: &mut tungstenite::WebSocket<_>, value: Value| socket.send(Message::text(value.to_string()));
    let since = Activity::now_ms();
    // The user's status from Discord, sent back unchanged.
    let mut status = "online".to_owned();
    let mut ready = false;
    let mut sequence = Value::Null;
    let mut heartbeat: Option<(Duration, Instant)> = None;
    let mut acknowledged = true;
    let mut idle_since = None;
    let ended = loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                if !message["s"].is_null() { sequence = message["s"].clone(); }
                match message["op"].as_u64() {
                    Some(10) => {
                        let interval = Duration::from_millis(message["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250));
                        // Discord asks for the first heartbeat after a random fraction of the interval.
                        let jitter = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() as f64 / 1e9;
                        heartbeat = Some((interval, Instant::now() + interval.mul_f64(jitter)));
                        // Identify with "unknown" so Discord keeps the user's status; the song follows once READY reports it.
                        let identify = json!({"op": 2, "d": {
                            "token": current, "capabilities": 0, "compress": false,
                            "properties": {"os": "Linux", "browser": "Chrome", "device": ""},
                            "presence": presence(None, None, "unknown", since),
                        }});
                        if let Err(error) = send(&mut socket, identify) { break Ended::Failed(error.to_string()); }
                    }
                    Some(0) => match message["t"].as_str() {
                        Some("READY") => {
                            ready = true;
                            status = discord_status(&message["d"]["sessions"]).unwrap_or_else(|| "online".into());
                            if !state.art_pending && let Err(error) = send(&mut socket, state.update(&status, since)) {
                                break Ended::Failed(error.to_string());
                            }
                        }
                        // Picked up by the next update; resending now could echo back and forth with Discord.
                        Some("SESSIONS_REPLACE") => if let Some(new) = discord_status(&message["d"]) { status = new; },
                        _ => (),
                    },
                    Some(1) => if let Some((_, next)) = heartbeat.as_mut() { *next = Instant::now(); },
                    Some(11) => acknowledged = true,
                    Some(7) => break Ended::Failed("Discord asked to reconnect".into()),
                    Some(9) => break Ended::Failed("Discord invalidated the session".into()),
                    _ => (),
                }
            }
            Ok(Message::Close(frame)) => {
                let code = frame.map(|frame| u16::from(frame.code));
                break if code == Some(4004) { Ended::Rejected } else { Ended::Failed(format!("closed with code {code:?}")) };
            }
            Ok(_) => (),
            Err(tungstenite::Error::Io(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => (),
            Err(error) => break Ended::Failed(error.to_string()),
        }
        if let Some((interval, next)) = heartbeat.as_mut() && Instant::now() >= *next {
            if !acknowledged { break Ended::Failed("Discord stopped acknowledging heartbeats".into()); }
            acknowledged = false;
            *next = Instant::now() + *interval;
            if let Err(error) = send(&mut socket, json!({"op": 1, "d": sequence})) { break Ended::Failed(error.to_string()); }
        }
        let request = loop {
            match requests.try_recv() {
                Ok(Request::Token(new)) if new.as_deref().unwrap_or_default() != current => {
                    state.apply(Request::Token(new));
                    break Some(Ended::TokenChanged);
                }
                Ok(Request::Token(_)) => (),
                Ok(Request::Quit) | Err(mpsc::TryRecvError::Disconnected) => break Some(Ended::Quit),
                Ok(request) => {
                    state.apply(request);
                    if ready && !state.art_pending && let Err(error) = send(&mut socket, state.update(&status, since)) {
                        break Some(Ended::Failed(error.to_string()));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break None,
            }
        };
        if let Some(ended) = request { break ended; }
        if state.activity.is_some() { idle_since = None; }
        else if idle_since.get_or_insert_with(Instant::now).elapsed() >= LINGER { break Ended::Idle; }
    };
    if matches!(ended, Ended::Quit | Ended::Idle | Ended::TokenChanged) {
        // Clear the song and close normally, so Discord ends the session now instead of keeping it resumable.
        if ready { let _ = send(&mut socket, json!({"op": 3, "d": presence(None, None, &status, since)})); }
        let _ = socket.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }));
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match socket.read() {
                Err(tungstenite::Error::Io(error)) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => (),
                Err(_) => break,
                Ok(_) => (),
            }
        }
    }
    ended
}

const KEYRING: [&str; 4] = ["application", "oxidance", "service", "discord"];

/// The saved Discord token, if any.
pub fn load_token() -> Option<String> {
    let output = Command::new("secret-tool").arg("lookup").args(KEYRING).stderr(Stdio::null()).output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned()).filter(|token| !token.is_empty())
}

/// Saves the token in the system keyring, or forgets it when empty.
pub fn store_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        let _ = Command::new("secret-tool").arg("clear").args(KEYRING).stdout(Stdio::null()).stderr(Stdio::null()).status();
        return Ok(());
    }
    let mut child = Command::new("secret-tool").args(["store", "--label=Discord token"]).args(KEYRING)
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .map_err(|_| "secret-tool is required to store the Discord token in the system keyring.")?;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(token.as_bytes()).map_err(|error| error.to_string())?;
    drop(stdin);
    if !child.wait().map_err(|error| error.to_string())?.success() {
        return Err("Could not save the Discord token in the system keyring. Make sure it is unlocked.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_shows_the_song_not_the_player() {
        let activity = Activity { title: "Hello, World".into(), artist: Some("Artist".into()), start_ms: 1000, end_ms: Some(181_000), art_urls: vec![], speed: 1.5 };
        let value = presence(Some(&activity), Some("external/abc/https/example.com/cover.jpg"), "dnd", 5);
        let shown = &value["activities"][0];
        assert_eq!(shown["name"], "Artist (1.5× speed)", "the profile shows the speed");
        assert_eq!(shown["status_display_type"], 1, "the member list shows only the artist");
        assert_eq!(shown["details"], "Hello, World");
        assert_eq!(shown["type"], 2, "shown as Listening to");
        assert_eq!(shown["state"], "Artist");
        assert_eq!(shown["timestamps"]["end"], 181_000);
        assert_eq!(shown["assets"]["large_image"], "mp:external/abc/https/example.com/cover.jpg");
        assert_eq!(value["status"], "dnd", "the user's status is kept");
        assert_eq!(value["afk"], true, "away, so the phone still gets push notifications");
        assert!(!value.to_string().to_lowercase().contains("oxidance"));
        assert_eq!(presence(None, None, "online", 0)["activities"], json!([]));
        let long = Activity { title: "x".repeat(300), artist: None, start_ms: 0, end_ms: None, art_urls: vec![], speed: 1.0 };
        let value = presence(Some(&long), None, "online", 0);
        assert_eq!(value["activities"][0]["name"].as_str().unwrap().len(), 128, "the title stands in for a missing artist");
        assert_eq!(value["activities"][0]["status_display_type"], 2);
        let short = Activity { title: "Song".into(), ..long.clone() };
        assert_eq!(presence(Some(&short), None, "online", 0)["activities"][0]["name"], "Song (1× speed)");
        assert!(value["activities"][0].get("state").is_none() && value["activities"][0]["timestamps"].get("end").is_none());
        assert!(value["activities"][0].get("assets").is_none(), "no art, no placeholder image");
    }

    #[test]
    fn polling_jitter_is_not_resent() {
        let activity = Activity { title: "Song".into(), artist: None, start_ms: 10_000, end_ms: Some(70_000), art_urls: vec![], speed: 1.0 };
        assert!(activity.same(&Activity { start_ms: 10_400, end_ms: Some(70_400), ..activity.clone() }));
        assert!(!activity.same(&Activity { start_ms: 40_000, end_ms: Some(100_000), ..activity.clone() }), "a seek is resent");
        assert!(!activity.same(&Activity { title: "Other".into(), ..activity.clone() }));
        assert!(!activity.same(&Activity { speed: 1.25, ..activity.clone() }));
        assert!(!activity.same(&Activity { art_urls: vec!["https://example.com/cover.jpg".into()], ..activity.clone() }));
    }

    #[test]
    fn status_comes_from_the_combined_session() {
        let sessions = json!([{"session_id": "abc", "status": "online"}, {"session_id": "all", "status": "invisible"}]);
        assert_eq!(discord_status(&sessions).as_deref(), Some("invisible"));
        assert_eq!(discord_status(&json!([{"session_id": "abc", "status": "online"}])), None);
        assert_eq!(discord_status(&json!([{"session_id": "all", "status": "unknown"}])), None);
    }
}
