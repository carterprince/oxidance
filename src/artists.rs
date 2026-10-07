use std::{error::Error, time::Duration};
use serde_json::{Value, json};
use crate::{ArtistLink, Song, parse_song, text};

#[derive(Clone, Debug)]
pub struct Artist {
    pub link: ArtistLink,
    pub image_url: Option<String>,
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
    if google_image {
        if let Some((base, sizing)) = url.rsplit_once('=') {
            if sizing.starts_with('w') { return Some(format!("{base}=w320-h320-l90-rj")); }
        }
    }
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

fn parse_search(response: &Value, query: &str) -> Vec<Artist> {
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
    artists.sort_by_key(|artist| artist.link.name.trim().to_lowercase() != query.trim().to_lowercase());
    artists.truncate(5);
    artists
}

pub fn search(query: &str) -> Result<Vec<Artist>, Box<dyn Error>> {
    let response = request("search", json!({"query": query, "params": "EgWKAQIgAWoMEA4QChADEAQQCRAF"}))?;
    Ok(parse_search(&response, query))
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
        assert_eq!(profile_image_url(&banner).as_deref(), Some("https://yt3.googleusercontent.com/portrait=w320-h320-l90-rj"));
    }

    #[test]
    fn exact_artist_match_precedes_other_results() {
        let row = |id: &str, name: &str| json!({"musicResponsiveListItemRenderer": {
            "navigationEndpoint": {"browseEndpoint": {"browseId": id}},
            "flexColumns": [{"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": name}]}}}]
        }});
        let response = json!({"contents": [row("UCother", "Other"), row("UCnujabes", "Nujabes"), row("UCnujabes", "Nujabes"), row("MPREalbum", "Album")]});
        let artists = parse_search(&response, " nujabes ");
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].link.name, "Nujabes");
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
