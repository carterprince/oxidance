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
        Some("i.ytimg.com") if parsed.path().starts_with("/vi/") =>
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
        if parsed.host_str() == Some("i.ytimg.com") && parsed.path().starts_with("/vi/") {
            if let Some((base, _)) = url.rsplit_once('/') { return format!("{base}/maxresdefault.jpg"); }
        }
    }
    url.to_owned()
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
    Some(Song { video_id: video_id.to_owned(), title, artist: (!artists.is_empty()).then(|| artists.join(", ")), album_art_url, artists: links })
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
