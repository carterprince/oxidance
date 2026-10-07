//! Minimal WebDAV client for syncing with an rclone `serve webdav` server.
use std::{collections::HashMap, io::{Read, Write}, path::Path, sync::{Mutex, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

use reqwest::{Method, StatusCode, Url, blocking::{Client, RequestBuilder, Response}};

pub const SONGS: &str = "songs/";
const LISTING_LIFETIME: Duration = Duration::from_secs(60);

pub struct Dav {
    client: Client,
    pub url: Url,
    pub username: String,
    password: String,
    listing: Mutex<Option<(Instant, HashMap<String, u64>)>>,
}

pub fn normalize_url(value: &str) -> Result<Url, String> {
    let value = value.trim();
    let value = if value.contains("://") { value.to_owned() } else { format!("https://{value}") };
    let url = Url::parse(&format!("{}/", value.trim_end_matches('/'))).map_err(|_| "Enter a valid server URL.")?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() || !url.username().is_empty()
        || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("Use an HTTP or HTTPS URL without credentials, query, or fragment.".into());
    }
    Ok(url)
}

fn method(name: &str) -> Method { Method::from_bytes(name.as_bytes()).unwrap() }

fn check(response: Response, action: &str) -> Result<Response, String> {
    match response.status() {
        status if status.is_success() => Ok(response),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN =>
            Err("The sync server rejected the username or password.".into()),
        status => Err(format!("{action} failed: HTTP {}", status.as_u16())),
    }
}

impl Dav {
    pub fn new(url: Url, username: String, password: String) -> Result<Self, String> {
        // Audio transfers can take minutes, so only small requests get an overall timeout.
        let client = Client::builder().connect_timeout(Duration::from_secs(10)).timeout(None)
            .build().map_err(|error| error.to_string())?;
        Ok(Self { client, url, username, password, listing: Mutex::new(None) })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client.request(method, self.url.join(path).unwrap())
            .basic_auth(&self.username, Some(&self.password))
    }

    fn send(&self, request: RequestBuilder, action: &str) -> Result<Response, String> {
        request.send().map_err(|error| format!("Could not reach the sync server: {error}"))
            .and_then(|response| check(response, action))
    }

    /// Connects to `url`, or to its `oxidance/` folder when `url` is just a domain.
    pub fn connect(url: &str, username: String, password: String) -> Result<Self, String> {
        let url = normalize_url(url)?;
        let mut candidates = vec![url.clone()];
        if url.path() == "/" { candidates.push(url.join("oxidance/").unwrap()); }
        let mut error = String::new();
        for url in candidates {
            let dav = Self::new(url, username.clone(), password.clone())?;
            match dav.probe() {
                Ok(()) => return Ok(dav),
                Err(failure) => error = failure,
            }
        }
        Err(error)
    }

    /// Confirms the URL is a WebDAV folder and the credentials are accepted.
    pub fn probe(&self) -> Result<(), String> {
        let response = self.request(method("PROPFIND"), "").timeout(Duration::from_secs(30)).header("Depth", "0").send()
            .map_err(|error| format!("Could not reach the sync server: {error}"))?;
        let response = check(response, "Connecting")?;
        if response.status() != StatusCode::MULTI_STATUS {
            return Err("This address is not a WebDAV folder. Check the server URL.".into());
        }
        Ok(())
    }

    pub fn get(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self.request(Method::GET, path).timeout(Duration::from_secs(60)).send()
            .map_err(|error| format!("Could not reach the sync server: {error}"))?;
        if response.status() == StatusCode::NOT_FOUND { return Ok(None); }
        let response = check(response, &format!("Downloading {path}"))?;
        response.bytes().map(|bytes| Some(bytes.to_vec())).map_err(|error| error.to_string())
    }

    pub fn put(&self, path: &str, bytes: Vec<u8>, lock: Option<&str>) -> Result<(), String> {
        let mut request = self.request(Method::PUT, path).timeout(Duration::from_secs(60)).body(bytes);
        if let Some(token) = lock { request = request.header("If", format!("({token})")); }
        self.send(request, &format!("Uploading {path}")).map(|_| ())
    }

    pub fn mkcol(&self, path: &str) -> Result<(), String> {
        let response = self.request(method("MKCOL"), path).timeout(Duration::from_secs(30)).send()
            .map_err(|error| format!("Could not reach the sync server: {error}"))?;
        // 405 means the collection already exists.
        if response.status() == StatusCode::METHOD_NOT_ALLOWED { return Ok(()); }
        check(response, &format!("Creating {path}")).map(|_| ())
    }

    /// Takes an exclusive write lock, waiting while another device holds it.
    pub fn lock(&self, path: &str) -> Result<String, String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>oxidance</D:owner></D:lockinfo>"#;
        for _ in 0..10 {
            let response = self.request(method("LOCK"), path).timeout(Duration::from_secs(30))
                .header("Timeout", "Second-120").header("Depth", "0")
                .header("Content-Type", "application/xml").body(BODY).send()
                .map_err(|error| format!("Could not reach the sync server: {error}"))?;
            if response.status() == StatusCode::LOCKED {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            let response = check(response, &format!("Locking {path}"))?;
            return response.headers().get("Lock-Token").and_then(|value| value.to_str().ok())
                .map(str::to_owned).ok_or_else(|| "The sync server did not return a lock token.".into());
        }
        Err("Another device is still syncing. Try again shortly.".into())
    }

    pub fn unlock(&self, path: &str, token: &str) -> Result<(), String> {
        let request = self.request(method("UNLOCK"), path).timeout(Duration::from_secs(30)).header("Lock-Token", token);
        self.send(request, &format!("Unlocking {path}")).map(|_| ())
    }

    /// Lists file names and sizes directly inside a collection; a missing collection is empty.
    pub fn list(&self, path: &str) -> Result<HashMap<String, u64>, String> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/></D:prop></D:propfind>"#;
        let response = self.request(method("PROPFIND"), path).timeout(Duration::from_secs(60))
            .header("Depth", "1").header("Content-Type", "application/xml").body(BODY).send()
            .map_err(|error| format!("Could not reach the sync server: {error}"))?;
        if response.status() == StatusCode::NOT_FOUND { return Ok(HashMap::new()); }
        let response = check(response, &format!("Listing {}", if path.is_empty() { "server" } else { path }))?;
        if response.status() != StatusCode::MULTI_STATUS {
            return Err("The sync server did not return a WebDAV listing. Check the server URL.".into());
        }
        let body = response.bytes().map_err(|error| error.to_string())?;
        parse_listing(&body)
    }

    /// The `songs/` listing, cached briefly so many queued downloads share one request.
    pub fn songs(&self, refresh: bool) -> Result<HashMap<String, u64>, String> {
        let mut cache = self.listing.lock().unwrap();
        if !refresh && let Some((time, listing)) = cache.as_ref() && time.elapsed() < LISTING_LIFETIME {
            return Ok(listing.clone());
        }
        let listing = self.list(SONGS)?;
        *cache = Some((Instant::now(), listing.clone()));
        Ok(listing)
    }

    /// Streams a remote file to `destination` through a temporary file.
    pub fn download(&self, path: &str, destination: &Path, cancelled: &AtomicBool) -> Result<(), String> {
        let mut response = self.send(self.request(Method::GET, path), &format!("Downloading {path}"))?;
        let mut name = destination.file_name().unwrap().to_owned();
        name.push(".part");
        let temporary = destination.with_file_name(name);
        let result = (|| {
            let mut file = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
            let mut buffer = vec![0; 1 << 16];
            loop {
                if cancelled.load(Ordering::Relaxed) { return Err("Download cancelled".to_owned()); }
                let read = response.read(&mut buffer).map_err(|error| format!("Download from sync server interrupted: {error}"))?;
                if read == 0 { break; }
                file.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
            }
            file.sync_all().map_err(|error| error.to_string())?;
            std::fs::rename(&temporary, destination).map_err(|error| error.to_string())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&temporary); }
        result
    }

    /// Uploads a local file under a temporary name, then moves it into place so
    /// other devices never see a partial file.
    pub fn upload(&self, path: &str, source: &Path) -> Result<(), String> {
        let file = std::fs::File::open(source).map_err(|error| error.to_string())?;
        let length = file.metadata().map_err(|error| error.to_string())?.len();
        let temporary = format!("{path}.part");
        let body = reqwest::blocking::Body::sized(file, length);
        self.send(self.request(Method::PUT, &temporary).body(body), &format!("Uploading {path}"))?;
        let destination = self.url.join(path).unwrap();
        let request = self.request(method("MOVE"), &temporary).timeout(Duration::from_secs(60))
            .header("Destination", destination.as_str()).header("Overwrite", "T");
        self.send(request, &format!("Uploading {path}"))?;
        if let Some(name) = path.strip_prefix(SONGS) && let Some((_, listing)) = self.listing.lock().unwrap().as_mut() {
            listing.insert(name.to_owned(), length);
        }
        Ok(())
    }
}

fn parse_listing(body: &[u8]) -> Result<HashMap<String, u64>, String> {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_reader(body);
    let mut buffer = Vec::new();
    let mut entries = HashMap::new();
    let (mut element, mut href, mut length, mut collection) = (Vec::new(), String::new(), 0, false);
    loop {
        match reader.read_event_into(&mut buffer).map_err(|error| format!("Invalid server listing: {error}"))? {
            Event::Start(start) => {
                element = start.local_name().as_ref().to_vec();
                if element == b"response" { (href, length, collection) = (String::new(), 0, false); }
                if element == b"collection" { collection = true; }
            }
            Event::Empty(empty) if empty.local_name().as_ref() == b"collection" => collection = true,
            Event::Text(text) => {
                let text = String::from_utf8_lossy(&text);
                match element.as_slice() {
                    b"href" => href.push_str(text.trim()),
                    b"getcontentlength" => length = text.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
            Event::End(end) => {
                element.clear();
                if end.local_name().as_ref() == b"response" && !collection {
                    if let Some(name) = href.trim_end_matches('/').rsplit('/').next().filter(|name| !name.is_empty()) {
                        entries.insert(name.to_owned(), length);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_normalized() {
        assert_eq!(normalize_url("example.org/oxidance").unwrap().as_str(), "https://example.org/oxidance/");
        assert_eq!(normalize_url("http://127.0.0.1:8081/").unwrap().as_str(), "http://127.0.0.1:8081/");
        assert!(normalize_url("ftp://example.org").is_err());
        assert!(normalize_url("https://user:secret@example.org").is_err());
    }

    #[test]
    fn listing_skips_collections() {
        let body = br#"<?xml version="1.0" encoding="UTF-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/oxidance/songs/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
<D:response><D:href>/oxidance/songs/abc-_1.webm</D:href><D:propstat><D:prop><D:resourcetype></D:resourcetype><D:getcontentlength>1234</D:getcontentlength></D:prop></D:propstat></D:response>
<D:response><D:href>/oxidance/songs/abc-_1.cover</D:href><D:propstat><D:prop><D:resourcetype/><D:getcontentlength>5</D:getcontentlength></D:prop></D:propstat></D:response>
</D:multistatus>"#;
        let listing = parse_listing(body).unwrap();
        assert_eq!(listing.len(), 2);
        assert_eq!(listing["abc-_1.webm"], 1234);
        assert_eq!(listing["abc-_1.cover"], 5);
    }
}
