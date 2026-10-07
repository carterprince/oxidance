<p align="center">
  <img src="data/icons/scalable/apps/io.github.oxidance.Oxidance.svg" width="128" height="128" alt="Oxidance app icon">
</p>

# Oxidance

A native music player for Linux, built with Rust, GTK 4, and libadwaita. Search
YouTube Music, keep a local library of liked songs and playlists, and listen
offline. Optionally, sync your library and downloads across devices with your
own WebDAV server.

Oxidance is an unofficial client and is not affiliated with YouTube or Google.

## Features

- **Search:** find songs, artists, and albums. Suggestions appear as you type,
  even for partial names, followed by full results that load as you scroll.
  Artist pages show artwork, a biography, and top songs; album pages show the
  artwork, artists, and track list.
- **Library:** like songs and organize them into playlists. Your library is
  stored locally in `~/.local/share/oxidance/library.json`, not in a YouTube
  account.
- **Offline listening:** liked songs and playlist songs are downloaded to
  `~/Music` in their original audio format, with album artwork. Saved songs play
  from disk; everything else streams.
- **Playback:** queue, shuffle, seeking, and a 0.5×–2× speed control. Press
  Space to play or pause.
- **Desktop integration:** media keys and desktop media controls via MPRIS.
- **Sync (optional):** keep your library and downloaded songs in sync across
  devices through a WebDAV server.

## Build and run

Requires Rust/Cargo, GTK 4.6+, libadwaita 1.6+, GStreamer with its HTTP,
WebM/Opus, and SoundTouch plugins, `pkg-config`, `glib-compile-resources`, and
`glib-compile-schemas`. Downloads use `yt-dlp` with Node.js; sync uses
`secret-tool` to store the password.

On Fedora:

```sh
sudo dnf install rust cargo gtk4-devel libadwaita-devel glib2-devel pkgconf-pkg-config \
    gstreamer1-devel gstreamer1-plugins-good gstreamer1-plugins-bad-free \
    yt-dlp nodejs libsecret
```

Build and launch:

```sh
cargo run --release
```

To build and install Oxidance for your user, with its desktop entry and icon
(the binary goes to `~/.local/bin`; set `PREFIX` to change that):

```sh
./install.sh
```

## Sync

Open **Options → Connect Sync Server…** and enter your WebDAV server address,
username, and password. If you enter only a domain, Oxidance also tries its
`/oxidance/` folder.

- Oxidance is local-first: everything is saved on your device, and sync runs in
  the background. It syncs on launch, shortly after library changes, when the
  window regains focus, and every minute.
- Likes and playlists from all devices are merged. Deleting a playlist on one
  device deletes it everywhere.
- Downloaded songs and artwork are uploaded to the server. Other devices download
  them from the server, falling back to YouTube only when needed.
- Nothing is ever deleted from the server, and files in `~/Music` that are not in
  your library are never uploaded.
- The icon in the header shows sync progress or problems; click it for details.

The server must support WebDAV locks. [rclone](https://rclone.org/)'s
`serve webdav` works well, for example behind a reverse proxy such as Caddy:

```sh
rclone serve webdav ~/oxidance --addr 127.0.0.1:8081 --baseurl /oxidance --user NAME --pass PASSWORD
```

## Tests

```sh
cargo test
# Sync against a temporary local rclone server.
cargo test --bin oxidance sync_merges -- --ignored
# UI tests need a display; run each one separately.
cargo test --bin oxidance -- --ignored --list
```
