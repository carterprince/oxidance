//! Discord rich presence sent straight to Discord's gateway as the user, so the Discord app
//! does not need to be running. The activity is named after the artist, never the player.

use std::{cell::RefCell, collections::HashMap, io::{ErrorKind, Write}, process::{Command, Stdio}, sync::mpsc, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

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
}

impl Activity {
    pub fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
    }

    /// Whether Discord already shows this, ignoring the timeline jitter of polling the player.
    fn same(&self, other: &Self) -> bool {
        let close = |a: i64, b: i64| (a - b).abs() < 2000;
        self.title == other.title && self.artist == other.artist && self.art_urls == other.art_urls && close(self.start_ms, other.start_ms)
            && match (self.end_ms, other.end_ms) { (Some(a), Some(b)) => close(a, b), (a, b) => a == b }
    }
}

/// Discord rejects activity text longer than 128 characters.
fn clip(text: &str) -> String { text.chars().take(128).collect() }

/// `art` is the image's media proxy path, from `Art::path`.
fn presence(activity: Option<&Activity>, art: Option<&str>) -> Value {
    let activities: Vec<Value> = activity.into_iter().map(|activity| {
        // The name is what the member list shows after the music note. Artists are more recognizable
        // than song titles, as in Discord's Spotify presence.
        let mut value = json!({
            "name": clip(activity.artist.as_deref().unwrap_or(&activity.title)), "type": 2, "details": clip(&activity.title),
            "timestamps": {"start": activity.start_ms}, "application_id": APPLICATION_ID,
        });
        if let Some(art) = art { value["assets"] = json!({"large_image": format!("mp:{art}")}); }
        if let Some(artist) = &activity.artist { value["state"] = clip(artist).into(); }
        if let Some(end) = activity.end_ms { value["timestamps"]["end"] = end.into(); }
        value
    }).collect();
    json!({"since": 0, "afk": false, "status": "online", "activities": activities})
}

/// Media proxy paths of album art Discord has already registered, and whether each image loads.
#[derive(Default)]
struct Art(HashMap<String, String>, HashMap<String, bool>);

impl Art {
    /// Whether `url` loads. Discord shows a broken image for a missing one, such as a
    /// YouTube video without a high-resolution thumbnail.
    fn loads(&mut self, url: &str) -> bool {
        *self.1.entry(url.to_owned()).or_insert_with(|| reqwest::blocking::Client::new().head(url).timeout(Duration::from_secs(10))
            .send().is_ok_and(|response| response.status().is_success()))
    }

    fn path(&mut self, token: &str, url: &str) -> Option<String> {
        if let Some(path) = self.0.get(url) { return Some(path.clone()); }
        let response = reqwest::blocking::Client::new()
            .post(format!("https://discord.com/api/v9/applications/{APPLICATION_ID}/external-assets"))
            .header("Authorization", token).json(&json!({"urls": [url]})).timeout(Duration::from_secs(10)).send()
            .and_then(|response| response.error_for_status()).and_then(|response| response.json::<Value>());
        let path = match response {
            Ok(value) => value[0]["external_asset_path"].as_str()?.to_owned(),
            Err(error) => { eprintln!("Discord could not load album art: {error}"); return None; }
        };
        self.0.insert(url.to_owned(), path.clone());
        Some(path)
    }

    fn presence(&mut self, token: &str, activity: Option<&Activity>) -> Value {
        let url = activity.and_then(|activity| activity.art_urls.iter().find(|url| self.loads(url)));
        let art = url.and_then(|url| self.path(token, url));
        presence(activity, art.as_deref())
    }
}

enum Request { Token(Option<String>), Activity(Option<Activity>), Quit }

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
        std::thread::Builder::new().name("discord".into()).spawn(move || { let _running = running; run(receiver, errors) })
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

fn run(requests: mpsc::Receiver<Request>, errors: async_channel::Sender<String>) {
    let mut token = None;
    let mut activity = None;
    let mut backoff = Duration::from_secs(1);
    let mut art = Art::default();
    // Returns false when the thread should stop.
    let apply = |request, token: &mut Option<String>, activity: &mut Option<Activity>| match request {
        Request::Token(new) => { *token = new.filter(|new: &String| !new.is_empty()); true }
        Request::Activity(new) => { *activity = new; true }
        Request::Quit => false,
    };
    loop {
        while token.is_none() || activity.is_none() {
            match requests.recv() {
                Ok(request) => if !apply(request, &mut token, &mut activity) { return; },
                Err(_) => return,
            }
        }
        match session(&requests, &mut token, &mut activity, &mut art) {
            Ended::Quit => return,
            Ended::Idle | Ended::TokenChanged => backoff = Duration::from_secs(1),
            Ended::Rejected => {
                token = None;
                let _ = errors.send_blocking("Discord rejected the token. Enter a new one in Preferences.".into());
            }
            Ended::Failed(error) => {
                eprintln!("Discord presence disconnected: {error}");
                let deadline = Instant::now() + backoff;
                while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                    match requests.recv_timeout(left) {
                        Ok(request) => if !apply(request, &mut token, &mut activity) { return; },
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                backoff = (backoff * 2).min(LONGEST_BACKOFF);
            }
        }
    }
}

fn session(requests: &mpsc::Receiver<Request>, token: &mut Option<String>, activity: &mut Option<Activity>, art: &mut Art) -> Ended {
    let current = token.clone().unwrap_or_default();
    let mut socket = match tungstenite::connect(GATEWAY) { Ok((socket, _)) => socket, Err(error) => return Ended::Failed(error.to_string()) };
    // Wake regularly to heartbeat and pick up new songs.
    let timeout = Some(Duration::from_millis(250));
    let _ = match socket.get_ref() {
        MaybeTlsStream::Rustls(stream) => stream.get_ref().set_read_timeout(timeout),
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
        _ => Ok(()),
    };
    let send = |socket: &mut tungstenite::WebSocket<_>, value: Value| socket.send(Message::text(value.to_string()));
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
                        let identify = json!({"op": 2, "d": {
                            "token": current, "capabilities": 0, "compress": false,
                            "properties": {"os": "Linux", "browser": "Chrome", "device": ""},
                            "presence": art.presence(&current, activity.as_ref()),
                        }});
                        if let Err(error) = send(&mut socket, identify) { break Ended::Failed(error.to_string()); }
                    }
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
                    *token = new.filter(|new| !new.is_empty());
                    break Some(Ended::TokenChanged);
                }
                Ok(Request::Token(_)) => (),
                Ok(Request::Activity(new)) => {
                    *activity = new;
                    if heartbeat.is_some() && let Err(error) = send(&mut socket, json!({"op": 3, "d": art.presence(&current, activity.as_ref())})) {
                        break Some(Ended::Failed(error.to_string()));
                    }
                }
                Ok(Request::Quit) | Err(mpsc::TryRecvError::Disconnected) => break Some(Ended::Quit),
                Err(mpsc::TryRecvError::Empty) => break None,
            }
        };
        if let Some(ended) = request { break ended; }
        if activity.is_some() { idle_since = None; }
        else if idle_since.get_or_insert_with(Instant::now).elapsed() >= LINGER { break Ended::Idle; }
    };
    if matches!(ended, Ended::Quit | Ended::Idle | Ended::TokenChanged) {
        // Clear the song and close normally, so Discord ends the session now instead of keeping it resumable.
        if heartbeat.is_some() { let _ = send(&mut socket, json!({"op": 3, "d": presence(None, None)})); }
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
        let activity = Activity { title: "Hello, World".into(), artist: Some("Artist".into()), start_ms: 1000, end_ms: Some(181_000), art_urls: vec![] };
        let value = presence(Some(&activity), Some("external/abc/https/example.com/cover.jpg"));
        let shown = &value["activities"][0];
        assert_eq!(shown["name"], "Artist", "the member list shows the artist");
        assert_eq!(shown["details"], "Hello, World");
        assert_eq!(shown["type"], 2, "shown as Listening to");
        assert_eq!(shown["state"], "Artist");
        assert_eq!(shown["timestamps"]["end"], 181_000);
        assert_eq!(shown["assets"]["large_image"], "mp:external/abc/https/example.com/cover.jpg");
        assert!(!value.to_string().to_lowercase().contains("oxidance"));
        assert_eq!(presence(None, None)["activities"], json!([]));
        let long = Activity { title: "x".repeat(300), artist: None, start_ms: 0, end_ms: None, art_urls: vec![] };
        let value = presence(Some(&long), None);
        assert_eq!(value["activities"][0]["name"].as_str().unwrap().len(), 128, "the title stands in for a missing artist");
        assert!(value["activities"][0].get("state").is_none() && value["activities"][0]["timestamps"].get("end").is_none());
        assert!(value["activities"][0].get("assets").is_none(), "no art, no placeholder image");
    }

    #[test]
    fn polling_jitter_is_not_resent() {
        let activity = Activity { title: "Song".into(), artist: None, start_ms: 10_000, end_ms: Some(70_000), art_urls: vec![] };
        assert!(activity.same(&Activity { start_ms: 10_400, end_ms: Some(70_400), ..activity.clone() }));
        assert!(!activity.same(&Activity { start_ms: 40_000, end_ms: Some(100_000), ..activity.clone() }), "a seek is resent");
        assert!(!activity.same(&Activity { title: "Other".into(), ..activity.clone() }));
        assert!(!activity.same(&Activity { art_urls: vec!["https://example.com/cover.jpg".into()], ..activity.clone() }));
    }
}
