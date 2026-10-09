use std::{collections::{HashMap, HashSet}, path::{Path, PathBuf}, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::Duration};
use oxidance::Song;
use crate::dav::{Dav, SONGS};

/// The connected sync server, shared with download and sync threads.
pub type Remote = Arc<Mutex<Option<Arc<Dav>>>>;

const EXTENSIONS: &[&str] = &["webm", "m4a", "mp4", "ogg", "opus", "mp3", "aac", "flac", "wav"];

/// Saved audio files by video ID, from a single scan of the music directory.
#[derive(Default)]
pub struct Index(HashMap<String, PathBuf>);

impl Index {
    pub fn scan(directory: &Path) -> Self {
        let mut files = HashMap::new();
        let Ok(entries) = std::fs::read_dir(directory) else { return Self(files); };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(id) = audio_id(&path) else { continue; };
            if files.contains_key(id) { continue; }
            // The directory entry's type avoids a stat for each unrelated file.
            let is_file = entry.file_type().is_ok_and(|kind| kind.is_file() || kind.is_symlink() && path.is_file());
            if is_file && path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
                files.insert(id.to_owned(), path.clone());
            }
        }
        Self(files)
    }

    pub fn file(&self, id: &str) -> Option<&Path> { self.0.get(id).map(PathBuf::as_path) }

    pub fn art(&self, id: &str) -> Option<PathBuf> {
        let path = self.file(id)?.with_extension("cover");
        path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0).then_some(path)
    }
}

/// The video ID of a finished audio file named `… [ID].ext`.
fn audio_id(path: &Path) -> Option<&str> {
    path.extension().and_then(|ext| ext.to_str()).filter(|ext| EXTENSIONS.contains(ext))?;
    let stem = path.file_stem()?.to_str()?.strip_suffix(']')?;
    stem.rsplit_once('[').map(|(_, id)| id).filter(|id| !id.is_empty())
}

pub fn local_file(directory: &Path, id: &str) -> Option<PathBuf> {
    Index::scan(directory).file(id).map(Path::to_path_buf)
}

pub fn local_art(directory: &Path, id: &str) -> Option<PathBuf> { Index::scan(directory).art(id) }

fn save_art(audio: &Path, song: &Song, cancelled: &AtomicBool) -> Result<(), String> {
    let Some(url) = &song.album_art_url else { return Ok(()); };
    let path = audio.with_extension("cover");
    let preferred = oxidance::high_quality_art_url(url);
    let source = audio.with_extension("cover.source");
    let existing = path.metadata().is_ok_and(|metadata| metadata.len() > 0);
    if existing && (preferred == *url || std::fs::read_to_string(&source).is_ok_and(|saved| saved == preferred)) { return Ok(()); }
    if cancelled.load(Ordering::Relaxed) { return Err("Download cancelled".into()); }
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).build().map_err(|error| error.to_string())?;
    let fetch = |url: &str| oxidance::fetch_image(&client, url);
    let bytes = match fetch(&preferred).or_else(|_| fetch(url)) {
        Ok(bytes) => bytes,
        Err(_) if existing => return Ok(()), // Keep the usable offline cover when disconnected.
        Err(error) => return Err(error.to_string()),
    };
    // Validate the image before making it available to offline playback.
    gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(bytes.clone())).map_err(|error| error.to_string())?;
    if cancelled.load(Ordering::Relaxed) { return Err("Download cancelled".into()); }
    let temporary = path.with_extension("cover.part");
    std::fs::write(&temporary, &bytes).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
    std::fs::write(source, preferred).map_err(|error| error.to_string())
}

pub fn needs_art_upgrade(index: &Index, song: &Song) -> bool {
    let Some(url) = &song.album_art_url else { return false; };
    let preferred = oxidance::high_quality_art_url(url);
    let Some(audio) = index.file(&song.video_id) else { return false; };
    preferred != *url && !std::fs::read_to_string(audio.with_extension("cover.source")).is_ok_and(|saved| saved == preferred)
}

pub fn download(directory: &Path, song: &Song, remote: Option<&Dav>, cancelled: Arc<AtomicBool>) -> Result<PathBuf, String> {
    if song.video_id.is_empty() || !song.video_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err("Invalid song ID".into());
    }
    // Prefer the sync server's copy; YouTube remains the fallback when it is missing or unreachable.
    if local_file(directory, &song.video_id).is_none() && let Some(dav) = remote {
        if let Err(error) = fetch_from_server(dav, directory, song, &cancelled) {
            if cancelled.load(Ordering::Relaxed) { return Err(error); }
            eprintln!("Could not download {} from the sync server: {error}", song.video_id);
        }
    }
    let path = download_audio(directory, song, cancelled.clone())?;
    save_art(&path, song, &cancelled).map_err(|error| format!("Audio is saved, but album art failed: {error}"))?;
    if let Some(dav) = remote {
        // Failures are retried and reported by the next sync.
        let _ = dav.songs(false).and_then(|listing| upload_to_server(dav, &path, song, &listing));
    }
    Ok(path)
}

fn remote_audio<'a>(listing: &'a HashMap<String, u64>, id: &str) -> Option<&'a str> {
    listing.keys().map(String::as_str).find(|name| name.strip_prefix(id).and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|extension| EXTENSIONS.contains(&extension)))
}

fn local_name(song: &Song, extension: &str) -> String {
    let stem = match &song.artist { Some(artist) => format!("{artist} - {}", song.title), None => song.title.clone() };
    // Match yt-dlp's substitution for path separators, and stay well under filename limits.
    let mut stem: String = stem.chars().map(|character| if matches!(character, '/' | '\0') { '⧸' } else { character }).collect();
    while stem.len() > 180 { stem.pop(); }
    format!("{stem} [{}].{extension}", song.video_id)
}

fn fetch_from_server(dav: &Dav, directory: &Path, song: &Song, cancelled: &AtomicBool) -> Result<(), String> {
    let listing = dav.songs(false)?;
    let Some(name) = remote_audio(&listing, &song.video_id) else { return Ok(()); };
    let extension = name.rsplit_once('.').unwrap().1;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let audio = directory.join(local_name(song, extension));
    dav.download(&format!("{SONGS}{name}"), &audio, cancelled)?;
    for suffix in ["cover", "cover.source"] {
        let name = format!("{}.{suffix}", song.video_id);
        if listing.contains_key(&name) {
            // Artwork is optional here; save_art fetches it from the web if this fails.
            let _ = dav.download(&format!("{SONGS}{name}"), &audio.with_extension(suffix), cancelled);
        }
    }
    Ok(())
}

/// Uploads a saved song's audio and artwork if the server is missing them.
/// Returns whether anything was uploaded.
pub fn upload_to_server(dav: &Dav, audio: &Path, song: &Song, listing: &HashMap<String, u64>) -> Result<bool, String> {
    let id = &song.video_id;
    let mut uploaded = false;
    if remote_audio(listing, id).is_none() {
        let extension = audio.extension().unwrap().to_string_lossy();
        dav.upload(&format!("{SONGS}{id}.{extension}"), audio)?;
        uploaded = true;
    }
    let cover = audio.with_extension("cover");
    let source = audio.with_extension("cover.source");
    let Some(local_source) = std::fs::read_to_string(&source).ok() else {
        if cover.is_file() && !listing.contains_key(&format!("{id}.cover")) {
            dav.upload(&format!("{SONGS}{id}.cover"), &cover)?;
            uploaded = true;
        }
        return Ok(uploaded);
    };
    // Replace remote artwork only with the preferred high-quality version, so
    // devices with older artwork do not keep overwriting each other.
    let preferred = song.album_art_url.as_deref().is_some_and(|url| oxidance::high_quality_art_url(url) == local_source);
    let remote_source = listing.get(&format!("{id}.cover.source"));
    if cover.is_file() && (!listing.contains_key(&format!("{id}.cover"))
        || preferred && remote_source != Some(&(local_source.len() as u64))) {
        dav.upload(&format!("{SONGS}{id}.cover"), &cover)?;
        dav.upload(&format!("{SONGS}{id}.cover.source"), &source)?;
        uploaded = true;
    }
    Ok(uploaded)
}

/// Runs `attempt` up to `attempts` times, stopping early on success or cancellation.
fn with_retries<T>(attempts: u32, delay: Duration, cancelled: &AtomicBool, mut attempt: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut tries = 1;
    loop {
        match attempt() {
            Err(error) if tries < attempts && !cancelled.load(Ordering::Relaxed) => {
                eprintln!("Attempt {tries} of {attempts} failed, retrying: {error}");
                std::thread::sleep(delay);
                tries += 1;
            }
            result => return result,
        }
    }
}

fn download_audio(directory: &Path, song: &Song, cancelled: Arc<AtomicBool>) -> Result<PathBuf, String> {
    if let Some(path) = local_file(directory, &song.video_id) { return Ok(path); }
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let args: Vec<String> = [
        "--ignore-config", "--no-playlist", "--no-warnings", "--js-runtimes", "node",
        "--socket-timeout", "15", "--retries", "2", "--extractor-retries", "1",
        "--no-progress", "--no-overwrites", "-f", "bestaudio/best",
        "--paths",
    ].into_iter().map(str::to_owned).chain([
        directory.to_string_lossy().into_owned(), "-o".into(),
        // Name the file by the library's ID, which differs from the site's for songs added from links.
        format!("%(artist,uploader)s - %(title)s [{}].%(ext)s", song.video_id),
        "--".into(), song.page_url(),
    ]).collect();
    // YouTube intermittently rejects a download URL (for example, HTTP 403).
    // Each attempt runs yt-dlp again, which extracts fresh URLs.
    with_retries(3, Duration::from_secs(2), &cancelled, || {
        super::playback::run(&args, cancelled.clone(), Duration::from_secs(20 * 60))
    })?;
    local_file(directory, &song.video_id).ok_or_else(|| "Download finished but the audio file could not be found".into())
}

pub struct Queue {
    sender: std::sync::mpsc::Sender<Song>,
    pending: Arc<Mutex<HashSet<String>>>,
    cancelled: Arc<AtomicBool>,
}

impl Queue {
    pub fn new(directory: PathBuf, remote: Remote) -> (Self, async_channel::Receiver<(Song, Result<PathBuf, String>)>) {
        let (sender, receiver) = std::sync::mpsc::channel::<Song>();
        let (events, updates) = async_channel::unbounded();
        let pending = Arc::new(Mutex::new(HashSet::new()));
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_pending = pending.clone();
        let worker_cancelled = cancelled.clone();
        std::thread::spawn(move || {
            while let Ok(song) = receiver.recv() {
                if worker_cancelled.load(Ordering::Relaxed) { break; }
                let dav = remote.lock().unwrap().clone();
                let result = download(&directory, &song, dav.as_deref(), worker_cancelled.clone());
                worker_pending.lock().unwrap().remove(&song.video_id);
                if events.send_blocking((song, result)).is_err() { break; }
            }
        });
        (Self { sender, pending, cancelled }, updates)
    }

    pub fn enqueue(&self, song: &Song) {
        let mut pending = self.pending.lock().unwrap();
        if pending.insert(song.video_id.clone()) && self.sender.send(song.clone()).is_err() {
            pending.remove(&song.video_id);
        }
    }

    pub fn is_pending(&self, id: &str) -> bool {
        self.pending.lock().unwrap().contains(id)
    }
}

impl Drop for Queue {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Relaxed); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_cover_is_reused_without_network_or_audio_download() {
        let directory = std::env::temp_dir().join(format!("oxidance-cover-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let audio = directory.join("Song [example].webm");
        std::fs::write(&audio, "audio").unwrap();
        let cover = b"saved artwork".to_vec();
        std::fs::write(audio.with_extension("cover.part"), &cover).unwrap();
        assert!(local_art(&directory, "example").is_none());
        std::fs::write(audio.with_extension("cover"), &cover).unwrap();
        let song = Song { video_id: "example".into(), title: "Song".into(), artist: None,
            album_art_url: Some("http://127.0.0.1:9/unavailable".into()), artists: vec![], source_url: None };
        assert_eq!(download(&directory, &song, None, Arc::new(AtomicBool::new(false))).unwrap(), audio);
        assert_eq!(std::fs::read(local_art(&directory, "example").unwrap()).unwrap(), cover);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn index_maps_finished_audio_files_by_id() {
        let directory = std::env::temp_dir().join(format!("oxidance-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for (name, contents) in [("A - Song [one].webm", "audio"), ("A - Song [one].cover", "art"), ("B [two].m4a.part", "partial"),
            ("C [three].mp3", ""), ("Other [x] song.mp3", "audio"), ("Brackets [in] title [four].opus", "audio"), ("Plain.mp3", "audio")] {
            std::fs::write(directory.join(name), contents).unwrap();
        }
        let index = Index::scan(&directory);
        assert_eq!(index.file("one"), Some(directory.join("A - Song [one].webm").as_path()));
        assert_eq!(index.art("one"), Some(directory.join("A - Song [one].cover")));
        assert_eq!(index.file("four"), Some(directory.join("Brackets [in] title [four].opus").as_path()));
        for missing in ["two", "three", "x", "in"] { assert!(index.file(missing).is_none(), "{missing}"); }
        assert!(Index::scan(&directory.join("missing")).file("one").is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_downloads_are_retried_until_the_limit() {
        let cancelled = AtomicBool::new(false);
        let mut calls = 0;
        let result = with_retries(3, Duration::ZERO, &cancelled, || { calls += 1; if calls < 3 { Err("HTTP 403".to_owned()) } else { Ok(calls) } });
        assert_eq!(result, Ok(3));
        let mut calls = 0;
        let result: Result<(), _> = with_retries(3, Duration::ZERO, &cancelled, || { calls += 1; Err(format!("failure {calls}")) });
        assert_eq!(result, Err("failure 3".into()));
        cancelled.store(true, Ordering::Relaxed);
        let mut calls = 0;
        let _: Result<(), _> = with_retries(3, Duration::ZERO, &cancelled, || { calls += 1; Err("cancelled".to_owned()) });
        assert_eq!(calls, 1, "cancellation stops retries");
    }

    #[test]
    fn partial_and_empty_files_are_not_playable() {
        let directory = std::env::temp_dir().join(format!("oxidance-files-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("Song [example].webm.part"), "partial").unwrap();
        std::fs::write(directory.join("Song [example].webm"), "").unwrap();
        assert!(local_file(&directory, "example").is_none());
        std::fs::write(directory.join("Song [example].webm"), "audio").unwrap();
        assert!(local_file(&directory, "example").is_some());
        assert!(local_file(&directory, "other").is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
