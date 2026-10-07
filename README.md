# Oxidance

A Rust proof of concept using GTK4 and libadwaita. Search YouTube Music and
view songs in a scrollable list with titles, artists, and album art.
The header's search icon opens a search field in place of the window title.
The field hides after results arrive. Search starts automatically after one second without an input change;
Enter submits immediately. Results load in batches of 25; scrolling near
the bottom fetches the next batch. A failed request offers **Retry**.
Search also shows up to five artists before the songs, with exact artist-name
matches first. Click an artist result or a song's artist name to open a profile
with square artwork, an expandable biography, and up to 100 top songs from the
artist's linked song list (falling back to the preview if unavailable). The Back button returns to the previous
collection or search. Older saved songs resolve their artist name when clicked.

- Toggle a song's heart to like or unlike it.
- Use a song's **+** button to add it to an existing playlist, or create a new one.
- The left sidebar keeps **Liked songs** first, then playlists with the most
  recently modified at the top. Click a collection to browse it; the sidebar
  button in the header hides or shows the sidebar.
- Use **+** in the Playlists sidebar header to create an empty playlist. Within a
  playlist, **−** removes a song. Duplicate songs are not added twice.
- Right-click a playlist to rename or delete it. Deleting a playlist keeps liked
  songs and downloaded files.

Likes and playlists are local to this app and saved automatically in
`$XDG_DATA_HOME/oxidance/library.json` (normally
`~/.local/share/oxidance/library.json`). They are restored on subsequent launches.
They do not sync with a YouTube account. Click a song or its play button to stream
it from YouTube; click again to pause or resume. A playback control below the list
remains available while browsing other pages. Liking a song or adding it to a
playlist queues an audio download in its original format directly into `~/Music/`.
Files include the
YouTube video ID in their name, so likes and playlists share one download per song.
Album artwork is saved beside each audio file with the same name and a `.cover`
extension, retaining the original image format. Saved song rows and the playback
bar load this file first, so artwork remains visible offline. Existing downloads
with missing artwork are filled in on launch when internet is available.
Playback uses a completed local file when available and streams otherwise. A
download completing during playback takes effect on the next playback, without
interrupting the current song. The playback slider shows elapsed
time and duration; drag or click it to seek, including while paused. It is disabled
until the stream supports seeking. Removing likes or playlist entries keeps the
downloaded files. Missing or interrupted downloads for saved songs retry on launch;
yt-dlp can resume partial downloads. Download errors appear as toast messages.
The speedometer button opens a 0.5×–2× playback speed slider. Speed changes also
change pitch; elapsed and total times reflect the selected speed.
Speed processing uses SoundTouch resampling to avoid sample-skipping distortion.
Press Space to play or pause when you are not editing text.
Previous, next, and shuffle use a queue captured from the collection or search
results where playback started. Playback advances automatically when a song ends.
MPRIS exposes playback controls, shuffle, seeking, speed, volume, and track
metadata to desktop media controls. Downloaded artwork uses local file URLs,
so album covers remain visible in the desktop media panel offline.
Scroll the speed slider in 0.01 steps; its tick marks normal speed at 1.0×.
Playback speed is saved through GSettings/dconf at
`/io/github/oxidance/playback-speed` and restored on launch.

## Sync

Open **Options → Connect Sync Server…** and enter a WebDAV server URL, username,
and password (for example `https://example.org/oxidance/`; entering only the domain also tries its `/oxidance/` folder). The password is stored
in the system keyring with `secret-tool`; the URL and username are saved in GSettings.

- The app stays local-first: everything is saved on this device, and sync runs in
  the background on launch, a few seconds after library changes, when the window
  regains focus, every minute, and when the network returns.
- Likes and playlists are merged with the server's `library.json`, so changes from
  several devices combine. A deleted playlist stays deleted even if another device
  added songs to it. Playlists created separately with the same name are renamed
  with a number. The server copy is locked during each merge.
- Saved songs are stored on the server as `songs/<video ID>.<ext>` with their
  `.cover` artwork. Missing songs download from the server first and from YouTube
  only if the server does not have them; YouTube downloads are then uploaded.
- Nothing is deleted from the server. Songs outside the library in `~/Music` are
  never uploaded.
- If the local library is missing or empty, the server library is restored instead
  of being treated as deleted. If the local library cannot be loaded, sync pauses.
- The header icon shows sync progress, success, or a problem; click it for details
  or to sync now. Disconnecting keeps local files.

The server needs WebDAV locking, which rclone's `serve webdav` provides.

```sh
cargo run
```

Requires Rust, GTK 4.6+ and libadwaita 1.6+ development libraries, GStreamer
development libraries and runtime plugins (HTTP, WebM/Opus, SoundTouch/pitch, and audio output),
`pkg-config`, `yt-dlp`, Node.js for yt-dlp's JavaScript runtime, and network access.
These dependencies are installed on this machine. On Fedora, the GStreamer
development package is `gstreamer1-devel`.
Searches and image downloads run on a background thread so the window stays
responsive. Loading, empty results, and search errors appear in the window;
unavailable artwork keeps a placeholder icon.
Artwork is fetched as rows approach the visible area and cached for this session.

The original JSON CLI is also available:

```sh
cargo run --bin search -- "Daft Punk Get Lucky"
```

The shared search code uses YouTube Music's unofficial InnerTube endpoint with
the Songs filter. No API key, Python, or yt-dlp is needed. Multiple artists are
joined with commas, and missing metadata is represented by `null` in CLI output.
The JSON CLI still returns up to five songs. Searches with fewer matches return
fewer results. Rankings can vary
over time; this app uses English/US and searches without an account.
YouTube changes may require updates to the request or response parser.

```sh
cargo test
# Optional live UI test: opens a temporary window and fetches real artwork.
cargo test --bin oxidance ui_library_and_live_search -- --ignored
# Optional sync test against a temporary local rclone WebDAV server.
cargo test --bin oxidance sync_merges -- --ignored
```
