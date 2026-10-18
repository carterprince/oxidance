use std::{error::Error, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod library;
pub mod artists;

/// Lowercase words, ignoring punctuation and other symbols.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty()).map(str::to_owned).collect()
}

/// How many of the query's words appear in `text`. The last word may be
/// incomplete, so it also matches as a prefix.
pub fn match_score(query: &str, text: &str) -> usize {
    let query = words(query);
    let text = words(text);
    query.iter().enumerate().filter(|(index, word)| text.iter().any(|candidate| {
        candidate == *word || *index == query.len() - 1 && candidate.starts_with(word.as_str())
    })).count()
}

/// Requests a Google-hosted image at `size` pixels, keeping its other options. In
/// particular, `p` crops to the requested square; without it, wide images stay wide.
pub(crate) fn resize_google_image(url: &str, size: u32) -> String {
    let (base, options) = url.split_once('=').unwrap_or((url, ""));
    let is_size = |option: &str| option.len() > 1 && matches!(option.as_bytes()[0], b'w' | b'h' | b's')
        && option[1..].bytes().all(|byte| byte.is_ascii_digit());
    let mut parts = vec![format!("w{size}"), format!("h{size}")];
    parts.extend(options.split('-').filter(|option| !option.is_empty() && !is_size(option)).map(str::to_owned));
    for default in ["l90", "rj"] {
        if !parts.iter().any(|part| part == default) { parts.push(default.to_owned()); }
    }
    format!("{base}={}", parts.join("-"))
}

pub fn search_art_url(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else { return url.to_owned(); };
    match parsed.host_str() {
        Some("yt3.googleusercontent.com" | "lh3.googleusercontent.com" | "yt3.ggpht.com") =>
            resize_google_image(url, 240),
        Some("i.ytimg.com") if is_video_thumbnail(url) =>
            format!("{}/mqdefault.jpg", url.rsplit_once('/').unwrap().0),
        Some("i.scdn.co") => url.replace("ab67616d0000b273", "ab67616d00001e02"),
        _ => url.to_owned(),
    }
}

pub fn high_quality_art_url(url: &str) -> String {
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if parsed.host_str().is_some_and(|host| host == "yt3.googleusercontent.com" || host == "lh3.googleusercontent.com" || host == "yt3.ggpht.com") {
            return resize_google_image(url, 1200);
        }
    }
    if is_video_thumbnail(url) {
        if let Some((base, _)) = url.rsplit_once('/') { return format!("{base}/maxresdefault.jpg"); }
    }
    url.to_owned()
}

/// Whether `url` is a YouTube video frame rather than album art. For songs
/// uploaded as a still image, the square cover sits between black bars.
pub fn is_video_thumbnail(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| parsed.host_str() == Some("i.ytimg.com") && parsed.path().starts_with("/vi/"))
}

/// Crops a video frame to a square around the cover between its black bars.
/// The cover itself is never cropped or stretched: a cover that isn't square
/// keeps black at its sides. Images without bars, or whose content is more than
/// 4:3 from square, are returned unchanged.
pub fn trim_letterbox(bytes: Vec<u8>) -> Vec<u8> {
    use gtk::gdk_pixbuf::Pixbuf;
    let Ok(pixbuf) = Pixbuf::from_read(std::io::Cursor::new(bytes.clone())) else { return bytes; };
    let Some((x, y, width, height)) = letterbox_content(&pixbuf) else { return bytes; };
    let content = pixbuf.new_subpixbuf(x, y, width, height);
    let side = width.max(height);
    let square = if width == height { Some(content) } else {
        Pixbuf::new(content.colorspace(), content.has_alpha(), 8, side, side).inspect(|square| {
            square.fill(0x000000ff);
            content.copy_area(0, 0, width, height, square, (side - width) / 2, (side - height) / 2);
        })
    };
    let Some(cropped) = square else { return bytes; };
    let saved = if cropped.has_alpha() { cropped.save_to_bufferv("png", &[]) } else { cropped.save_to_bufferv("jpeg", &[("quality", "95")]) };
    saved.unwrap_or(bytes)
}

/// The content inside the image's dark borders, if it has any.
fn letterbox_content(pixbuf: &gtk::gdk_pixbuf::Pixbuf) -> Option<(i32, i32, i32, i32)> {
    let (width, height) = (pixbuf.width() as usize, pixbuf.height() as usize);
    let (stride, channels) = (pixbuf.rowstride() as usize, pixbuf.n_channels() as usize);
    let pixels = pixbuf.read_pixel_bytes();
    // JPEG noise keeps the bars from being exactly black.
    let dark = |x: usize, y: usize| pixels[y * stride + x * channels..][..3].iter().all(|&value| value <= 24);
    let left = (0..width).take_while(|&x| (0..height).all(|y| dark(x, y))).count();
    if left == width { return None; }
    let right = width - (0..width).rev().take_while(|&x| (0..height).all(|y| dark(x, y))).count();
    let top = (0..height).take_while(|&y| (left..right).all(|x| dark(x, y))).count();
    let bottom = height - (0..height).rev().take_while(|&y| (left..right).all(|x| dark(x, y))).count();
    let (content_width, content_height) = (right - left, bottom - top);
    let trimmed = content_width < width || content_height < height;
    // Old uploads often stretch the cover, so allow up to 4:3.
    let near_square = content_width.max(content_height) * 3 <= content_width.min(content_height) * 4;
    (trimmed && near_square).then_some((left as i32, top as i32, content_width as i32, content_height as i32))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ArtistLink {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album_art_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<ArtistLink>,
    /// The page of a song added from a link outside YouTube, such as Bandcamp.
    /// YouTube songs leave it empty and are found by `video_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// The part of a longer video this song is, such as one track of a full album.
    /// It plays from its own downloaded file, cut from the video's audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<Section>,
}

/// A span of a video's audio, in seconds.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub struct Section {
    pub start: f64,
    pub end: f64,
}

impl Song {
    /// The page that yt-dlp streams and downloads this song from.
    pub fn page_url(&self) -> String {
        self.source_url.clone().unwrap_or_else(|| format!("https://music.youtube.com/watch?v={}", self.video_id))
    }
}

/// Downloads an image, or reads it when `url` is a local `file://` URL (a cover
/// the user chose for a song added from a link).
pub fn fetch_image(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>, String> {
    if let Some(path) = reqwest::Url::parse(url).ok().filter(|url| url.scheme() == "file").and_then(|url| url.to_file_path().ok()) {
        return std::fs::read(path).map_err(|error| error.to_string());
    }
    let bytes = client.get(url).send().and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::bytes).map(|bytes| bytes.to_vec()).map_err(|error| error.to_string())?;
    Ok(if is_video_thumbnail(url) { trim_letterbox(bytes) } else { bytes })
}

pub(crate) fn text(value: &Value) -> String {
    value["runs"].as_array().map(|runs| {
        runs.iter().filter_map(|run| run["text"].as_str()).collect()
    }).unwrap_or_default()
}

pub(crate) fn parse_song(renderer: &Value) -> Option<Song> {
    // Exclude albums, playlists, and artists, even if another shelf is returned.
    let video_id = renderer.pointer("/playlistItemData/videoId").and_then(Value::as_str)
        .or_else(|| renderer.pointer("/navigationEndpoint/watchEndpoint/videoId").and_then(Value::as_str))
        .or_else(|| renderer.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId").and_then(Value::as_str))?;
    let columns = renderer["flexColumns"].as_array()?;
    let title = text(columns.first()?.pointer("/musicResponsiveListItemFlexColumnRenderer/text")?);
    if title.is_empty() { return None; }
    let mut artists = Vec::new();
    let mut links = Vec::new();
    for column in columns.iter().skip(1) {
        if let Some(runs) = column.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs").and_then(Value::as_array) {
            for run in runs {
                let id = run.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str);
                if id.is_some_and(|id| id.starts_with("UC") || id.starts_with("MPLA")) {
                    if let Some(name) = run["text"].as_str() {
                        if !artists.contains(&name) { artists.push(name); }
                        let link = ArtistLink { id: id.unwrap().to_owned(), name: name.to_owned() };
                        if !links.contains(&link) { links.push(link); }
                    }
                }
            }
        }
    }
    let album_art_url = renderer.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .and_then(|images| images.iter().filter(|image| image["url"].is_string())
            .max_by_key(|image| image["width"].as_u64().unwrap_or(0) * image["height"].as_u64().unwrap_or(0)))
        .and_then(|image| image["url"].as_str()).map(str::to_owned);
    Some(Song { video_id: video_id.to_owned(), title, artist: (!artists.is_empty()).then(|| artists.join(", ")), album_art_url, artists: links, source_url: None, section: None })
}

const PAGE_SIZE: usize = 25;

#[derive(Clone, Debug, Default)]
pub struct SearchCursor {
    pending: std::collections::VecDeque<Song>,
    continuation: Option<String>,
    visitor_data: Option<String>,
    seen: std::collections::HashSet<String>,
    used_tokens: std::collections::HashSet<String>,
    initialized: bool,
}

pub struct SearchPage {
    pub songs: Vec<Song>,
    pub next: Option<SearchCursor>,
}

fn continuation(value: &Value) -> Option<String> {
    value.pointer("/continuations/0/nextContinuationData/continuation")
        .or_else(|| value.pointer("/continuations/0/reloadContinuationData/continuation"))
        .or_else(|| value.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token"))
        .and_then(Value::as_str).map(str::to_owned)
}

fn parse_page(response: &Value) -> Result<(Vec<Song>, Option<String>), Box<dyn Error>> {
    let mut shelves = Vec::new();
    let mut appended = None;
    if let Some(shelf) = response.pointer("/continuationContents/musicShelfContinuation") {
        shelves.push(shelf);
    } else if let Some(commands) = response.get("onResponseReceivedCommands").and_then(Value::as_array) {
        appended = commands.iter().find_map(|command| command.pointer("/appendContinuationItemsAction/continuationItems").and_then(Value::as_array));
    } else {
        let contents = response.pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content")
            .or_else(|| response.get("contents")).ok_or("YouTube Music returned no search contents")?;
        let sections = contents.pointer("/sectionListRenderer/contents").and_then(Value::as_array)
            .ok_or("Unrecognized YouTube Music response; the unofficial API may have changed")?;
        for section in sections {
            if let Some(shelf) = section.get("musicShelfRenderer") {
                let category = text(&shelf["title"]);
                if category.is_empty() || category == "Songs" { shelves.push(shelf); }
            }
        }
    }
    let mut results = Vec::new();
    let mut next = None;
    for shelf in &shelves { next = continuation(shelf).or(next); }
    let entries = shelves.iter().filter_map(|shelf| shelf["contents"].as_array())
        .chain(appended).flatten();
    for entry in entries {
        if let Some(song) = parse_song(&entry["musicResponsiveListItemRenderer"]) { results.push(song); }
        next = continuation(entry).or(next);
    }
    Ok((results, next))
}

/// Fetch 25 unique songs, retaining overflow and continuation state for the next batch.
pub fn search_page(query: &str, cursor: Option<SearchCursor>) -> Result<SearchPage, Box<dyn Error>> {
    if query.trim().is_empty() { return Err("Enter a search query".into()); }
    let mut cursor = cursor.unwrap_or_default();
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build()?;
    for _ in 0..10 {
        if cursor.pending.len() >= PAGE_SIZE || (cursor.initialized && cursor.continuation.is_none()) { break; }
        let token = cursor.continuation.take();
        if let Some(token) = &token {
            if !cursor.used_tokens.insert(token.clone()) { break; }
        }
        let mut body = json!({
            "context": {"client": {"clientName": "WEB_REMIX", "clientVersion": "1.20261005.01.00", "hl": "en", "gl": "US"}},
            "query": query,
            "params": "EgWKAQIIAWoKEAoQAxAEEAkQBQ=="
        });
        if let Some(token) = token { body["continuation"] = json!(token); }
        if let Some(visitor) = &cursor.visitor_data { body["context"]["client"]["visitorData"] = json!(visitor); }
        let response: Value = client.post("https://music.youtube.com/youtubei/v1/search")
            .header("Origin", "https://music.youtube.com").header("User-Agent", "Mozilla/5.0")
            .json(&body).send()?.error_for_status()?.json()?;
        if let Some(error) = response.get("error") { return Err(format!("YouTube Music error: {error}").into()); }
        if let Some(visitor) = response.pointer("/responseContext/visitorData").and_then(Value::as_str) {
            cursor.visitor_data = Some(visitor.to_owned());
        }
        let (songs, next) = parse_page(&response)?;
        cursor.initialized = true;
        cursor.continuation = next.filter(|token| !cursor.used_tokens.contains(token));
        for song in songs {
            if cursor.seen.insert(song.video_id.clone()) { cursor.pending.push_back(song); }
        }
    }
    let songs = cursor.pending.drain(..cursor.pending.len().min(PAGE_SIZE)).collect();
    let has_more = !cursor.pending.is_empty() || cursor.continuation.is_some();
    Ok(SearchPage { songs, next: has_more.then_some(cursor) })
}

/// The JSON CLI retains its original five-result limit.
pub fn search(query: &str) -> Result<Vec<Song>, Box<dyn Error>> {
    Ok(search_page(query, None)?.songs.into_iter().take(5).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_bars_around_square_cover() {
        use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
        let frame = Pixbuf::new(Colorspace::Rgb, false, 8, 160, 90).unwrap();
        frame.fill(0x000000ff);
        frame.new_subpixbuf(35, 0, 90, 90).fill(0xe0c020ff);
        let trimmed = Pixbuf::from_read(std::io::Cursor::new(trim_letterbox(frame.save_to_bufferv("jpeg", &[]).unwrap()))).unwrap();
        assert!((88..=90).contains(&trimmed.width()) && trimmed.width() == trimmed.height(), "{}x{}", trimmed.width(), trimmed.height());

        // A wider cover keeps all of its width, with black above and below.
        frame.fill(0x000000ff);
        frame.new_subpixbuf(28, 0, 104, 90).fill(0xe0c020ff);
        frame.new_subpixbuf(28, 0, 8, 90).fill(0x2040e0ff);
        let trimmed = Pixbuf::from_read(std::io::Cursor::new(trim_letterbox(frame.save_to_bufferv("jpeg", &[]).unwrap()))).unwrap();
        assert_eq!((trimmed.width(), trimmed.height()), (104, 104));
        let pixels = trimmed.read_pixel_bytes();
        let row = 52 * trimmed.rowstride() as usize;
        assert!(pixels[row + 2] > 150 && pixels[row + 103 * 3] > 150, "the cover's edges must remain");
        assert!(pixels[..3].iter().all(|&value| value < 30), "black above the cover");

        // A narrower cover keeps its shape, with black at its sides.
        frame.fill(0x000000ff);
        frame.new_subpixbuf(40, 0, 80, 90).fill(0xe0c020ff);
        let trimmed = Pixbuf::from_read(std::io::Cursor::new(trim_letterbox(frame.save_to_bufferv("jpeg", &[]).unwrap()))).unwrap();
        assert_eq!((trimmed.width(), trimmed.height()), (90, 90));
        let pixels = trimmed.read_pixel_bytes();
        let row = 45 * trimmed.rowstride() as usize;
        assert!(pixels[row] < 30 && pixels[row + 45 * 3] > 150, "black sides around the unstretched cover");

        // A wide image with bars stays as it is.
        frame.fill(0x000000ff);
        frame.new_subpixbuf(10, 0, 140, 90).fill(0xe0c020ff);
        let bytes = frame.save_to_bufferv("jpeg", &[]).unwrap();
        assert_eq!(trim_letterbox(bytes.clone()), bytes);
    }

    fn song_renderer() -> Value {
        json!({
            "playlistItemData": {"videoId": "example"},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Song & title"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Artist A", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCa"}}},
                    {"text": " & "},
                    {"text": "Artist B", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCb"}}},
                    {"text": "Album", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREalbum"}}},
                    {"text": "3:45"}
                ]}}}
            ],
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://example.com/small.jpg", "width": 60, "height": 60},
                {"url": "https://example.com/large.jpg", "width": 120, "height": 120}
            ]}}}
        })
    }

    #[test]
    fn match_score_counts_query_words_in_the_text() {
        let query = "haruka nakamura let go";
        assert_eq!(match_score(query, "let go (feat. Nujabes) haruka nakamura"), 4);
        assert_eq!(match_score(query, "haruka nakamura"), 2);
        assert_eq!(match_score(query, "MELODICA Album • haruka nakamura • 2013"), 2);
        assert_eq!(match_score("piero pi", "Piero Piccioni"), 2, "the last word matches as a prefix");
        assert_eq!(match_score("pi piero", "Piccioni"), 0, "earlier words must be complete");
        assert_eq!(match_score("Café, del-mar!", "cafe del mar"), 2, "punctuation is ignored, accents are not");
        assert_eq!(match_score("", "anything"), 0);
    }

    #[test]
    fn resized_images_keep_their_crop_option() {
        let artist = "https://lh3.googleusercontent.com/photo=w120-h120-p-l90-rj";
        assert_eq!(search_art_url(artist), "https://lh3.googleusercontent.com/photo=w240-h240-p-l90-rj");
        assert_eq!(high_quality_art_url(artist), "https://lh3.googleusercontent.com/photo=w1200-h1200-p-l90-rj");
        let cover = "https://lh3.googleusercontent.com/cover=w120-h120-l90-rj";
        assert_eq!(search_art_url(cover), "https://lh3.googleusercontent.com/cover=w240-h240-l90-rj", "unchanged without options");
        assert_eq!(high_quality_art_url("https://yt3.ggpht.com/plain=s88"), "https://yt3.ggpht.com/plain=w1200-h1200-l90-rj");
        assert_eq!(search_art_url("https://yt3.googleusercontent.com/bare"), "https://yt3.googleusercontent.com/bare=w240-h240-l90-rj");
    }

    #[test]
    fn artist_names_exclude_album_and_duration_and_choose_largest_art() {
        let song = parse_song(&song_renderer()).unwrap();
        assert_eq!(song.title, "Song & title");
        assert_eq!(song.artist.as_deref(), Some("Artist A, Artist B"));
        assert_eq!(song.album_art_url.as_deref(), Some("https://example.com/large.jpg"));
    }

    #[test]
    fn search_results_preserve_order() {
        let entries: Vec<_> = (0..7).map(|index| {
            let mut renderer = song_renderer();
            renderer["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0]["text"] = json!(format!("Song {index}"));
            json!({"musicResponsiveListItemRenderer": renderer})
        }).collect();
        let response = json!({"contents": {"sectionListRenderer": {"contents": [
            {"musicShelfRenderer": {"title": {"runs": [{"text": "Songs"}]}, "contents": entries}}
        ]}}});
        let (results, _) = parse_page(&response).unwrap();
        assert_eq!(results.len(), 7);
        assert_eq!(results[0].title, "Song 0");
        assert_eq!(results[4].title, "Song 4");
    }

    #[test]
    fn missing_metadata_keeps_song_and_non_song_entries_are_excluded() {
        let mut renderer = song_renderer();
        renderer["flexColumns"].as_array_mut().unwrap().truncate(1);
        renderer.as_object_mut().unwrap().remove("thumbnail");
        let song = parse_song(&renderer).unwrap();
        assert!(song.artist.is_none());
        assert!(song.album_art_url.is_none());
        renderer.as_object_mut().unwrap().remove("playlistItemData");
        assert!(parse_song(&renderer).is_none());
    }

    #[test]
    fn continuation_response_retains_token_and_songs() {
        let response = json!({"continuationContents": {"musicShelfContinuation": {
            "contents": [{"musicResponsiveListItemRenderer": song_renderer()}],
            "continuations": [{"nextContinuationData": {"continuation": "next-token"}}]
        }}});
        let (songs, next) = parse_page(&response).unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(next.as_deref(), Some("next-token"));
    }

    #[test]
    fn buffered_results_are_not_lost_between_batches() {
        let mut cursor = SearchCursor { initialized: true, ..Default::default() };
        for index in 0..60 {
            let mut song = parse_song(&song_renderer()).unwrap();
            song.video_id = index.to_string();
            cursor.pending.push_back(song);
        }
        let first = search_page("query", Some(cursor)).unwrap();
        assert_eq!(first.songs.len(), 25);
        assert_eq!(first.songs[24].video_id, "24");
        let second = search_page("query", first.next).unwrap();
        assert_eq!(second.songs.len(), 25);
        assert_eq!(second.songs[0].video_id, "25");
        let last = search_page("query", second.next).unwrap();
        assert_eq!(last.songs.len(), 10);
        assert!(last.next.is_none());
    }
}
