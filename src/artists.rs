use std::{error::Error, time::Duration};
use serde_json::{Value, json};
use crate::{ArtistLink, Song, parse_song, resize_google_image, text};

#[derive(Clone, Debug)]
pub struct Artist {
    pub link: ArtistLink,
    pub image_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Album {
    pub id: String,
    pub title: String,
    /// Kind, artist, and year as YouTube Music shows them, e.g. "Album • Piero Piccioni • 2022".
    pub subtitle: String,
    pub image_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AlbumPage {
    pub album: Album,
    pub artists: Vec<ArtistLink>,
    /// Kind, year, and length, e.g. "Album • 2022 · 40 songs • 2 hours, 6 minutes".
    pub details: String,
    pub songs: Vec<Song>,
}

/// A search-as-you-type entry. Unlike filtered search, suggestions match partial words.
#[derive(Clone, Debug)]
pub enum Suggestion {
    Artist(Artist),
    Album(Album),
    Song(Song),
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub artist: Artist,
    pub biography: Option<String>,
    pub songs: Vec<Song>,
}

fn request(endpoint: &str, mut body: Value) -> Result<Value, Box<dyn Error>> {
    body["context"] = json!({"client": {"clientName": "WEB_REMIX", "clientVersion": "1.20261005.01.00", "hl": "en", "gl": "US"}});
    let response: Value = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build()?
        .post(format!("https://music.youtube.com/youtubei/v1/{endpoint}"))
        .header("Origin", "https://music.youtube.com").header("User-Agent", "Mozilla/5.0")
        .json(&body).send()?.error_for_status()?.json()?;
    if let Some(error) = response.get("error") { return Err(format!("YouTube Music error: {error}").into()); }
    Ok(response)
}

fn image_url(value: &Value) -> Option<String> {
    value.as_array()?.iter().filter(|image| image["url"].is_string())
        .max_by_key(|image| image["width"].as_u64().unwrap_or(0) * image["height"].as_u64().unwrap_or(0))?
        ["url"].as_str().map(str::to_owned)
}

fn profile_image_url(images: &Value) -> Option<String> {
    let square: Vec<_> = images.as_array()?.iter().filter(|image| {
        image["width"].as_u64().is_some_and(|width| width > 0 && Some(width) == image["height"].as_u64())
    }).cloned().collect();
    if !square.is_empty() { return image_url(&Value::Array(square)); }
    let url = image_url(images)?;
    let google_image = reqwest::Url::parse(&url).ok().and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host.ends_with(".googleusercontent.com") || host.ends_with(".ggpht.com"));
    if google_image && url.contains('=') { return Some(resize_google_image(&url, 320)); }
    Some(url)
}

fn collect<'a>(value: &'a Value, key: &str, output: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            if let Some(found) = object.get(key) { output.push(found); }
            for child in object.values() { collect(child, key, output); }
        }
        Value::Array(array) => for child in array { collect(child, key, output); },
        _ => (),
    }
}

fn parse_search(response: &Value) -> Vec<Artist> {
    let mut rows = Vec::new();
    collect(&response["contents"], "musicResponsiveListItemRenderer", &mut rows);
    let mut seen = std::collections::HashSet::new();
    let mut artists = Vec::new();
    for row in rows {
        let Some(id) = row.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str) else { continue; };
        if !(id.starts_with("UC") || id.starts_with("MPLA")) || !seen.insert(id.to_owned()) { continue; }
        let name = text(&row["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]);
        if name.is_empty() { continue; }
        artists.push(Artist { link: ArtistLink { id: id.to_owned(), name },
            image_url: image_url(&row["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]) });
    }
    artists.truncate(5);
    artists
}

fn page_type(row: &Value) -> Option<&str> {
    row.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
        .and_then(Value::as_str)
}

fn column(row: &Value, index: usize) -> String {
    text(&row["flexColumns"][index]["musicResponsiveListItemFlexColumnRenderer"]["text"])
}

fn parse_album(row: &Value) -> Option<Album> {
    let id = row.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str)?;
    let title = column(row, 0);
    if !id.starts_with("MPRE") || title.is_empty() { return None; }
    Some(Album { id: id.to_owned(), title, subtitle: column(row, 1),
        image_url: image_url(&row["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]) })
}

/// Suggestions in YouTube Music's order, keeping artists, albums, and songs.
fn parse_suggestions(response: &Value) -> Vec<Suggestion> {
    let mut rows = Vec::new();
    collect(&response["contents"], "musicResponsiveListItemRenderer", &mut rows);
    rows.into_iter().filter_map(|row| match page_type(row) {
        Some("MUSIC_PAGE_TYPE_ARTIST") => {
            let id = row.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str)?;
            let name = column(row, 0);
            (id.starts_with("UC") && !name.is_empty()).then(|| Suggestion::Artist(Artist { link: ArtistLink { id: id.to_owned(), name },
                image_url: image_url(&row["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]) }))
        }
        Some("MUSIC_PAGE_TYPE_ALBUM") => parse_album(row).map(Suggestion::Album),
        // Audio tracks only, matching the Songs search filter; skip music videos and uploads.
        _ if row.pointer("/navigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")
            .and_then(Value::as_str) == Some("MUSIC_VIDEO_TYPE_ATV") => parse_song(row).map(Suggestion::Song),
        _ => None,
    }).collect()
}

pub fn suggestions(query: &str) -> Result<Vec<Suggestion>, Box<dyn Error>> {
    Ok(parse_suggestions(&request("music/get_search_suggestions", json!({"input": query}))?))
}

pub fn search_albums(query: &str) -> Result<Vec<Album>, Box<dyn Error>> {
    let response = request("search", json!({"query": query, "params": "EgWKAQIYAWoMEA4QChADEAQQCRAF"}))?;
    let mut rows = Vec::new();
    collect(&response["contents"], "musicResponsiveListItemRenderer", &mut rows);
    let mut albums: Vec<Album> = Vec::new();
    for album in rows.into_iter().filter_map(parse_album) {
        if !albums.iter().any(|saved| saved.id == album.id) { albums.push(album); }
    }
    albums.truncate(3);
    Ok(albums)
}

fn parse_album_page(response: &Value, id: &str) -> Result<AlbumPage, Box<dyn Error>> {
    let mut headers = Vec::new();
    collect(&response["contents"], "musicResponsiveHeaderRenderer", &mut headers);
    let header = headers.first().ok_or("Album is unavailable or has an unsupported layout")?;
    let title = text(&header["title"]);
    if title.is_empty() { return Err("Album has no title".into()); }
    let artists: Vec<ArtistLink> = header.pointer("/straplineTextOne/runs").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|run| {
            let id = run.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str)?;
            Some(ArtistLink { id: id.to_owned(), name: run["text"].as_str()?.to_owned() })
        }).collect();
    let artist_names = if artists.is_empty() { text(&header["straplineTextOne"]) }
        else { artists.iter().map(|artist| artist.name.as_str()).collect::<Vec<_>>().join(", ") };
    let kind = text(&header["subtitle"]);
    let details = [kind.as_str(), &text(&header["secondSubtitle"])].into_iter().filter(|part| !part.is_empty())
        .collect::<Vec<_>>().join(" · ");
    let mut thumbnails = Vec::new();
    collect(&header["thumbnail"], "thumbnails", &mut thumbnails);
    let album = Album { id: id.to_owned(), title, subtitle: [kind.split(" • ").next().unwrap_or(""), &artist_names].into_iter()
            .filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" • "),
        image_url: thumbnails.first().and_then(|images| image_url(images)) };
    let mut shelves = Vec::new();
    collect(&response["contents"], "musicShelfRenderer", &mut shelves);
    let mut songs = Vec::new();
    for row in shelves.iter().filter_map(|shelf| shelf["contents"].as_array()).flatten() {
        // Unavailable tracks have no video and are skipped.
        let Some(mut song) = parse_song(&row["musicResponsiveListItemRenderer"]) else { continue; };
        if song.artists.is_empty() && !artists.is_empty() {
            song.artist = Some(artist_names.clone());
            song.artists = artists.clone();
        }
        // Album tracks have no artwork of their own.
        if song.album_art_url.is_none() { song.album_art_url = album.image_url.clone(); }
        if !songs.iter().any(|saved: &Song| saved.video_id == song.video_id) { songs.push(song); }
    }
    Ok(AlbumPage { album, artists, details, songs })
}

pub fn album(id: &str) -> Result<AlbumPage, Box<dyn Error>> {
    parse_album_page(&request("browse", json!({"browseId": id}))?, id)
}

pub fn search(query: &str) -> Result<Vec<Artist>, Box<dyn Error>> {
    let response = request("search", json!({"query": query, "params": "EgWKAQIgAWoMEA4QChADEAQQCRAF"}))?;
    Ok(parse_search(&response))
}

fn parse_profile(response: &Value, id: &str) -> Result<Profile, Box<dyn Error>> {
    let header = response.pointer("/header/musicImmersiveHeaderRenderer")
        .or_else(|| response.pointer("/header/musicVisualHeaderRenderer"))
        .or_else(|| response.pointer("/header/musicHeaderRenderer"))
        .ok_or("Artist profile is unavailable or has an unsupported layout")?;
    let name = text(&header["title"]);
    if name.is_empty() { return Err("Artist profile has no name".into()); }
    let artist = Artist { link: ArtistLink { id: id.to_owned(), name },
        image_url: profile_image_url(&header["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"]) };
    let mut descriptions = Vec::new();
    collect(&response["contents"], "musicDescriptionShelfRenderer", &mut descriptions);
    let biography = descriptions.iter().map(|shelf| text(&shelf["description"]))
        .find(|description| !description.trim().is_empty());
    let mut shelves = Vec::new();
    collect(&response["contents"], "musicShelfRenderer", &mut shelves);
    let mut songs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for shelf in shelves {
        if !matches!(text(&shelf["title"]).as_str(), "Top songs" | "Songs") { continue; }
        if let Some(rows) = shelf["contents"].as_array() {
            for row in rows {
                if let Some(mut song) = parse_song(&row["musicResponsiveListItemRenderer"]) {
                    if !seen.insert(song.video_id.clone()) { continue; }
                    if song.artists.is_empty() {
                        song.artist = Some(artist.link.name.clone());
                        song.artists.push(artist.link.clone());
                    }
                    songs.push(song);
                }
            }
        }
    }
    Ok(Profile { artist, biography, songs })
}

pub fn profile(id: &str) -> Result<Profile, Box<dyn Error>> {
    let response = request("browse", json!({"browseId": id.strip_prefix("MPLA").unwrap_or(id)}))?;
    let mut profile = parse_profile(&response, id)?;
    let mut shelves = Vec::new();
    collect(&response["contents"], "musicShelfRenderer", &mut shelves);
    let endpoint = shelves.iter().filter(|shelf| matches!(text(&shelf["title"]).as_str(), "Top songs" | "Songs"))
        .find_map(|shelf| shelf.pointer("/title/runs/0/navigationEndpoint/browseEndpoint"));
    if let Some(endpoint) = endpoint {
        if let Some(browse_id) = endpoint["browseId"].as_str() {
            let mut body = json!({"browseId": browse_id});
            if let Some(params) = endpoint["params"].as_str() { body["params"] = json!(params); }
            // A failed full-list request still leaves the artist's preview available.
            if let Ok(full) = request("browse", body) {
                let mut rows = Vec::new();
                collect(&full["contents"], "musicPlaylistShelfRenderer", &mut rows);
                collect(&full["contents"], "musicShelfRenderer", &mut rows);
                let mut seen: std::collections::HashSet<_> = profile.songs.iter().map(|song| song.video_id.clone()).collect();
                for shelf in rows {
                    if profile.songs.len() >= 100 { break; }
                    if let Some(entries) = shelf["contents"].as_array() {
                        for entry in entries {
                            if let Some(mut song) = parse_song(&entry["musicResponsiveListItemRenderer"]) {
                                if !seen.insert(song.video_id.clone()) { continue; }
                                if song.artists.is_empty() {
                                    song.artist = Some(profile.artist.link.name.clone());
                                    song.artists.push(profile.artist.link.clone());
                                }
                                profile.songs.push(song);
                                if profile.songs.len() >= 100 { break; }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_prefers_square_portraits_and_leaves_square_images_unchanged() {
        let square = json!([{"url": "https://example.com/square.jpg", "width": 320, "height": 320},
            {"url": "https://example.com/banner.jpg", "width": 2880, "height": 1200}]);
        assert_eq!(profile_image_url(&square).as_deref(), Some("https://example.com/square.jpg"));
        let banner = json!([{"url": "https://yt3.googleusercontent.com/portrait=w2880-h1200-p-l90-rj", "width": 2880, "height": 1200}]);
        assert_eq!(profile_image_url(&banner).as_deref(), Some("https://yt3.googleusercontent.com/portrait=w320-h320-p-l90-rj"));
    }

    #[test]
    fn artist_results_keep_youtube_order_without_duplicates() {
        let row = |id: &str, name: &str| json!({"musicResponsiveListItemRenderer": {
            "navigationEndpoint": {"browseEndpoint": {"browseId": id}},
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": name}]}}}]
        }});
        let response = json!({"contents": [row("UCother", "Other"), row("UCnujabes", "Nujabes"), row("UCnujabes", "Nujabes"), row("MPREalbum", "Album")]});
        let artists = parse_search(&response);
        assert_eq!(artists.iter().map(|artist| artist.link.name.as_str()).collect::<Vec<_>>(), ["Other", "Nujabes"]);
    }

    fn suggestion_row(title: &str, subtitle: Value, navigation: Value) -> Value {
        json!({"musicResponsiveListItemRenderer": {
            "navigationEndpoint": navigation,
            "playlistItemData": navigation.pointer("/watchEndpoint/videoId").map(|id| json!({"videoId": id})),
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": subtitle}}}
            ]
        }})
    }

    fn browse(id: &str, page: &str) -> Value {
        json!({"browseEndpoint": {"browseId": id, "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": page}}}})
    }

    fn watch(id: &str, kind: &str) -> Value {
        json!({"watchEndpoint": {"videoId": id, "watchEndpointMusicSupportedConfigs": {"watchEndpointMusicConfig": {"musicVideoType": kind}}}})
    }

    #[test]
    fn suggestions_keep_artists_albums_and_songs() {
        let artist_run = json!([{"text": "Song"}, {"text": " • "}, {"text": "Piero Piccioni", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCpiero"}}}]);
        let response = json!({"contents": [{"searchSuggestionsSectionRenderer": {"contents": [
            suggestion_row("Piero Piccioni", json!([{"text": "101M monthly audience"}]), browse("UCpiero", "MUSIC_PAGE_TYPE_ARTIST")),
            suggestion_row("Greatest Hits", json!([{"text": "Album • Piero Piccioni • 2022"}]), browse("MPREb_hits", "MUSIC_PAGE_TYPE_ALBUM")),
            suggestion_row("Easy Lovers", artist_run, watch("easy", "MUSIC_VIDEO_TYPE_ATV")),
            suggestion_row("Live video", json!([{"text": "Video • 1M views"}]), watch("video", "MUSIC_VIDEO_TYPE_OMV")),
            suggestion_row("Mix", json!([{"text": "Playlist • YouTube Music"}]), browse("VLmix", "MUSIC_PAGE_TYPE_PLAYLIST")),
        ]}}]});
        let suggestions = parse_suggestions(&response);
        assert_eq!(suggestions.len(), 3, "music videos and playlists are skipped");
        assert!(matches!(&suggestions[0], Suggestion::Artist(artist) if artist.link.id == "UCpiero"));
        assert!(matches!(&suggestions[1], Suggestion::Album(album) if album.subtitle == "Album • Piero Piccioni • 2022"));
        assert!(matches!(&suggestions[2], Suggestion::Song(song) if song.artists[0].name == "Piero Piccioni"));
    }

    #[test]
    fn album_page_fills_track_artists_and_artwork() {
        let track = |id: &str, title: &str, artists: Value| json!({"musicResponsiveListItemRenderer": {
            "playlistItemData": {"videoId": id},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": artists}}}
            ]
        }});
        let unavailable = json!({"musicResponsiveListItemRenderer": {"flexColumns": [
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Gone"}]}}}]}});
        let response = json!({"contents": {"twoColumnBrowseResultsRenderer": {
            "tabs": [{"content": {"musicResponsiveHeaderRenderer": {
                "title": {"runs": [{"text": "Greatest Hits"}]},
                "subtitle": {"runs": [{"text": "Album • 2022"}]},
                "straplineTextOne": {"runs": [{"text": "Piero Piccioni", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCpiero"}}}]},
                "secondSubtitle": {"runs": [{"text": "2 songs • 7 minutes"}]},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                    {"url": "https://example.com/small.jpg", "width": 60, "height": 60},
                    {"url": "https://example.com/large.jpg", "width": 544, "height": 544}]}}}
            }}}],
            "secondaryContents": {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": [
                track("one", "Easy Lovers", json!([])),
                unavailable,
                track("two", "Duet", json!([{"text": "Guest", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCguest"}}}])),
            ]}}]}}
        }}});
        let page = parse_album_page(&response, "MPREb_hits").unwrap();
        assert_eq!(page.album.title, "Greatest Hits");
        assert_eq!(page.album.subtitle, "Album • Piero Piccioni");
        assert_eq!(page.details, "Album • 2022 · 2 songs • 7 minutes");
        assert_eq!(page.album.image_url.as_deref(), Some("https://example.com/large.jpg"));
        assert_eq!(page.songs.len(), 2, "unavailable tracks are skipped");
        assert_eq!(page.songs[0].artist.as_deref(), Some("Piero Piccioni"));
        assert_eq!(page.songs[0].artists[0].id, "UCpiero");
        assert_eq!(page.songs[1].artist.as_deref(), Some("Guest"));
        assert!(page.songs.iter().all(|song| song.album_art_url.as_deref() == Some("https://example.com/large.jpg")));
    }

    #[test]
    #[ignore = "requires live YouTube Music access"]
    fn live_suggestions_and_album() {
        let suggestions = suggestions("piero pi").unwrap();
        assert!(suggestions.iter().any(|item| matches!(item, Suggestion::Artist(artist) if artist.link.name == "Piero Piccioni")), "{suggestions:?}");
        let albums = search_albums("piero piccioni").unwrap();
        assert!(!albums.is_empty());
        let page = album(&albums[0].id).unwrap();
        assert!(!page.songs.is_empty());
        assert!(page.songs.iter().all(|song| !song.artists.is_empty() && song.album_art_url.is_some()));
    }

    #[test]
    fn old_saved_songs_accept_missing_artist_links() {
        let song: Song = serde_json::from_value(json!({"video_id": "old", "title": "Old", "artist": "Artist", "album_art_url": null})).unwrap();
        assert!(song.artists.is_empty());
    }

    #[test]
    #[ignore = "requires live YouTube Music access"]
    fn live_artist_search_and_profile() {
        let artists = search("nujabes").unwrap();
        assert_eq!(artists[0].link.name.to_lowercase(), "nujabes");
        let profile = profile(&artists[0].link.id).unwrap();
        assert_eq!(profile.artist.link.name, "Nujabes");
        assert!(profile.artist.image_url.is_some());
        assert!(profile.biography.is_some());
        assert!(profile.songs.len() > 5, "full top-songs list should extend the five-song preview");
        assert!(profile.songs.iter().all(|song| !song.artists.is_empty()));
    }
}
