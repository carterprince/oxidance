//! Syncs the library and saved songs with a WebDAV server. The app stays
//! local-first: everything is saved locally, and sync runs in the background.
use std::{io::Write, path::Path, process::{Command, Stdio}, rc::Weak, sync::Arc};

use serde::{Deserialize, Serialize};
use super::*;
use crate::dav::{Dav, SONGS, normalize_url};

const LIBRARY: &str = "library.json";

enum Event { Library(Library), Progress(String) }

/// The library as last synced with a particular server, used as the merge base.
#[derive(Deserialize, Serialize)]
struct Base { url: String, library: Library }

fn load_base(path: &Path, url: &str) -> Result<Option<Library>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Base>(&bytes).map(|base| (base.url == url).then_some(base.library))
            .map_err(|_| "Sync history is damaged. Disconnect and reconnect to sync again.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn save_base(path: &Path, url: &str, library: &Library) -> Result<(), String> {
    let bytes = serde_json::to_vec(&Base { url: url.to_owned(), library: library.clone() }).map_err(|error| error.to_string())?;
    // A new device has no data directory until its library is first saved.
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).and_then(|_| std::fs::rename(&temporary, path)).map_err(|error| error.to_string())
}

fn saved_songs(library: &Library) -> Vec<Song> {
    let mut ids = std::collections::HashSet::new();
    library.liked.iter().chain(library.playlists.iter().flat_map(|playlist| &playlist.songs))
        .filter(|song| ids.insert(song.video_id.clone())).cloned().collect()
}

/// One sync pass: merge the library under a server lock, then upload songs the server lacks.
/// Songs the device lacks are fetched by the download queue once the merged library is applied.
fn cycle(dav: &Dav, local: &Library, base_path: &Path, music: &Path, events: &async_channel::Sender<Event>) -> Result<String, String> {
    let url = dav.url.to_string();
    let base = load_base(base_path, &url)?;
    dav.mkcol(SONGS)?;
    let token = dav.lock(LIBRARY)?;
    let result = (|| {
        let remote = match dav.get(LIBRARY)? {
            Some(bytes) if !bytes.is_empty() => Some(serde_json::from_slice::<Library>(&bytes)
                .map_err(|_| "The library on the sync server is damaged, so it was left unchanged.")?),
            _ => None,
        };
        // A missing remote library or an emptied local one must not be read as
        // "everything was deleted"; combine both sides instead.
        let base = base.filter(|_| remote.is_some() && !local.is_empty());
        let merged = Library::merge(base.as_ref(), local, remote.as_ref().unwrap_or(&Library::default()));
        if remote.as_ref() != Some(&merged) {
            dav.put(LIBRARY, serde_json::to_vec_pretty(&merged).map_err(|error| error.to_string())?, Some(&token))?;
        }
        Ok::<_, String>(merged)
    })();
    let _ = dav.unlock(LIBRARY, &token);
    let merged = result?;
    save_base(base_path, &url, &merged)?;
    let _ = events.send_blocking(Event::Library(merged.clone()));
    let listing = dav.songs(true)?;
    let songs = saved_songs(&merged);
    let files = downloads::Index::scan(music);
    let mut uploaded = 0;
    for (index, song) in songs.iter().enumerate() {
        let _ = events.send_blocking(Event::Progress(format!("Checking saved songs ({} of {})…", index + 1, songs.len())));
        let Some(audio) = files.file(&song.video_id) else { continue; };
        if downloads::upload_to_server(dav, audio, song, &listing)? { uploaded += 1; }
    }
    Ok(match uploaded {
        0 => "Library and songs are up to date.".into(),
        1 => "Uploaded 1 song.".into(),
        count => format!("Uploaded {count} songs."),
    })
}

fn keyring(url: &str, username: &str, password: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("secret-tool");
    match password {
        Some(_) => command.args(["store", "--label=Oxidance sync server"]),
        None => command.arg("lookup"),
    };
    command.args(["application", "oxidance", "url", url, "username", username]);
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
        .map_err(|_| "secret-tool is required to store the sync password in the system keyring.")?;
    let mut stdin = child.stdin.take().unwrap();
    if let Some(password) = password { stdin.write_all(password.as_bytes()).map_err(|error| error.to_string())?; }
    drop(stdin);
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() && password.is_some() {
        return Err("Could not save the password in the system keyring. Make sure it is unlocked.".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_owned())
}

pub struct Controller {
    ui: Weak<Ui>,
    button: gtk::Button,
    icon: gtk::Image,
    spinner: adw::Spinner,
    actions: Vec<gtk::gio::SimpleAction>,
    running: Cell<bool>,
    again: Cell<bool>,
    generation: Cell<u64>,
    debounce: RefCell<Option<glib::SourceId>>,
    detail: RefCell<String>,
}

pub fn install(ui: &Rc<Ui>, button: &gtk::Button, menu: &gtk::gio::Menu) {
    let icon = gtk::Image::new();
    let spinner = adw::Spinner::builder().visible(false).build();
    let icons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    icons.append(&icon);
    icons.append(&spinner);
    button.set_child(Some(&icons));
    let section = gtk::gio::Menu::new();
    section.append(Some("Connect Sync Server…"), Some("win.sync-connect"));
    section.append(Some("Sync Now"), Some("win.sync-now"));
    section.append(Some("Disconnect Sync Server"), Some("win.sync-disconnect"));
    menu.append_section(None, &section);
    let actions: Vec<_> = ["sync-connect", "sync-now", "sync-disconnect"].into_iter()
        .map(|name| gtk::gio::SimpleAction::new(name, None)).collect();
    let controller = Rc::new(Controller {
        ui: Rc::downgrade(ui), button: button.clone(), icon, spinner, actions,
        running: Cell::new(false), again: Cell::new(false), generation: Cell::new(0),
        debounce: RefCell::new(None), detail: RefCell::new(String::new()),
    });
    for action in &controller.actions {
        let weak = Rc::downgrade(&controller);
        action.connect_activate(move |action, _| {
            let Some(controller) = weak.upgrade() else { return; };
            match action.name().as_str() {
                "sync-connect" => controller.connect_dialog(),
                "sync-now" => controller.request(),
                _ => controller.disconnect(),
            }
        });
        ui.window.add_action(action);
    }
    button.connect_clicked({ let weak = Rc::downgrade(&controller); move |_| {
        if let Some(controller) = weak.upgrade() { controller.details_dialog(); }
    }});
    glib::timeout_add_seconds_local(60, { let weak = Rc::downgrade(&controller); move || {
        let Some(controller) = weak.upgrade() else { return glib::ControlFlow::Break; };
        controller.request();
        glib::ControlFlow::Continue
    }});
    ui.window.connect_is_active_notify({ let weak = Rc::downgrade(&controller); move |window| {
        if let Some(controller) = weak.upgrade() && window.is_active() { controller.request(); }
    }});
    gtk::gio::NetworkMonitor::default().connect_network_changed({ let weak = Rc::downgrade(&controller); move |_, available| {
        if let Some(controller) = weak.upgrade() && available { controller.request(); }
    }});
    *ui.sync.borrow_mut() = Some(controller.clone());
    controller.restore();
}

impl Controller {
    fn connected(&self) -> bool {
        self.ui.upgrade().is_some_and(|ui| ui.remote.lock().unwrap().is_some())
    }

    fn status(&self, icon: Option<&str>, detail: &str) {
        self.icon.set_visible(icon.is_some());
        if let Some(icon) = icon { self.icon.set_icon_name(Some(icon)); }
        self.spinner.set_visible(icon.is_none());
        self.button.set_tooltip_text(Some(detail));
        *self.detail.borrow_mut() = detail.to_owned();
        let connected = self.connected();
        for action in &self.actions {
            action.set_enabled(match action.name().as_str() {
                "sync-connect" => !connected,
                "sync-now" => connected && !self.running.get(),
                _ => connected,
            });
        }
    }

    fn restore(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let url = ui.settings.string("sync-url").to_string();
        let username = ui.settings.string("sync-username").to_string();
        if url.is_empty() {
            self.status(Some("network-offline-symbolic"), "Sync is not connected. Click to connect a WebDAV server.");
            return;
        }
        self.status(None, "Connecting to the sync server…");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gtk::gio::spawn_blocking(move || {
                let password = keyring(&url, &username, None)?;
                if password.is_empty() {
                    return Err("The sync password is not in the keyring. Disconnect and connect again.".to_owned());
                }
                Dav::new(normalize_url(&url)?, username, password)
            }).await.unwrap_or_else(|_| Err("Could not restore the sync connection.".into()));
            let Some(controller) = weak.upgrade() else { return; };
            if controller.generation.get() != generation { return; }
            match result {
                Ok(dav) => {
                    if let Some(ui) = controller.ui.upgrade() { *ui.remote.lock().unwrap() = Some(Arc::new(dav)); }
                    controller.request();
                }
                Err(error) => controller.status(Some("dialog-warning-symbolic"), &error),
            }
        });
    }

    /// Syncs after a short delay, so a burst of library changes uploads once.
    pub fn request_soon(self: &Rc<Self>) {
        if let Some(timer) = self.debounce.borrow_mut().take() { timer.remove(); }
        let weak = Rc::downgrade(self);
        *self.debounce.borrow_mut() = Some(glib::timeout_add_seconds_local_once(3, move || {
            if let Some(controller) = weak.upgrade() {
                controller.debounce.borrow_mut().take();
                controller.request();
            }
        }));
    }

    pub fn request(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let Some(dav) = ui.remote.lock().unwrap().clone() else { return; };
        if self.running.get() { self.again.set(true); return; }
        if !ui.writable {
            self.status(Some("dialog-warning-symbolic"), "Sync is paused because the saved library could not be loaded.");
            return;
        }
        self.running.set(true);
        self.again.set(false);
        self.status(None, "Syncing…");
        let snapshot = ui.library.borrow().clone();
        let local = snapshot.clone();
        let base_path = ui.path.with_file_name("sync-base.json");
        let music = ui.music_directory.clone();
        let (events, updates) = async_channel::unbounded();
        let (finished, result) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = finished.send_blocking(cycle(&dav, &local, &base_path, &music, &events));
        });
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = updates.recv().await {
                let Some(controller) = weak.upgrade() else { return; };
                if controller.generation.get() != generation { continue; }
                match event {
                    Event::Library(merged) => controller.apply(&snapshot, merged),
                    Event::Progress(detail) => controller.status(None, &detail),
                }
            }
            let result = result.recv().await.unwrap_or_else(|_| Err("Sync stopped unexpectedly.".into()));
            let Some(controller) = weak.upgrade() else { return; };
            if controller.generation.get() != generation { return; }
            controller.running.set(false);
            let time = glib::DateTime::now_local().ok().and_then(|now| now.format("%H:%M").ok())
                .map(|time| format!(" ({time})")).unwrap_or_default();
            match result {
                Ok(summary) => controller.status(Some("object-select-symbolic"), &format!("{summary}{time}")),
                Err(error) => controller.status(Some("dialog-warning-symbolic"), &format!("Sync failed{time}: {error}")),
            }
            if controller.again.get() { controller.request(); }
        });
    }

    /// Applies the merged library, keeping local changes made while sync was running.
    fn apply(&self, snapshot: &Library, merged: Library) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let current = ui.library.borrow().clone();
        let next = if current == *snapshot { merged } else {
            self.again.set(true);
            Library::merge(Some(snapshot), &current, &merged)
        };
        if next == current { return; }
        if let Err(error) = next.save(&ui.path) {
            ui.toast(&format!("Could not save synced library: {error}"));
            return;
        }
        *ui.library.borrow_mut() = next;
        let files = downloads::Index::scan(&ui.music_directory);
        for song in ui.saved_songs() { ui.queue_download(&song, &files); }
        if let View::Playlist(id) = ui.view.get() && !ui.library.borrow().playlists.iter().any(|playlist| playlist.id == id) {
            ui.navigate(View::Liked);
        } else {
            ui.refresh_sidebar();
            ui.render_preserving_scroll();
        }
    }

    fn details_dialog(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        if !self.connected() && !self.running.get() { self.connect_dialog(); return; }
        let dialog = adw::AlertDialog::builder().heading("Sync").body(self.detail.borrow().as_str()).build();
        dialog.add_response("close", "Close");
        dialog.add_response("sync", "Sync Now");
        dialog.set_response_enabled("sync", self.connected() && !self.running.get());
        dialog.set_close_response("close");
        dialog.connect_response(Some("sync"), { let weak = Rc::downgrade(self); move |_, _| {
            if let Some(controller) = weak.upgrade() { controller.request(); }
        }});
        dialog.present(Some(&ui.window));
    }

    fn connect_dialog(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let dialog = adw::AlertDialog::builder().heading("Connect Sync Server")
            .body("Sync your library and saved songs with a WebDAV server. The password is stored in the system keyring.").build();
        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let url = gtk::Entry::builder().placeholder_text("example.org/oxidance/").activates_default(true)
            .text(ui.settings.string("sync-url").as_str()).build();
        let username = gtk::Entry::builder().placeholder_text("Username").activates_default(true)
            .text(ui.settings.string("sync-username").as_str()).build();
        let password = gtk::PasswordEntry::builder().placeholder_text("Password").show_peek_icon(true).activates_default(true).build();
        for (label, field) in [("Server URL", url.upcast_ref::<gtk::Widget>()), ("Username", username.upcast_ref()), ("Password", password.upcast_ref())] {
            form.append(&gtk::Label::builder().label(label).xalign(0.0).css_classes(["caption-heading"]).build());
            form.append(field);
        }
        dialog.set_extra_child(Some(&form));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("connect", "Connect");
        dialog.set_default_response(Some("connect"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
        dialog.connect_response(Some("connect"), { let weak = Rc::downgrade(self); move |_, _| {
            if let Some(controller) = weak.upgrade() {
                controller.connect(url.text().to_string(), username.text().trim().to_owned(), password.text().to_string());
            }
        }});
        dialog.present(Some(&ui.window));
    }

    fn connect(self: &Rc<Self>, url: String, username: String, password: String) {
        self.generation.set(self.generation.get() + 1);
        self.status(None, "Connecting to the sync server…");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = gtk::gio::spawn_blocking(move || {
                let dav = Dav::connect(&url, username, password.clone())?;
                keyring(dav.url.as_str(), &dav.username, Some(&password))?;
                Ok::<_, String>(dav)
            }).await.unwrap_or_else(|_| Err("Could not connect.".into()));
            let Some(controller) = weak.upgrade() else { return; };
            let Some(ui) = controller.ui.upgrade() else { return; };
            if controller.generation.get() != generation { return; }
            match result {
                Ok(dav) => {
                    let saved = ui.settings.set_string("sync-url", dav.url.as_str())
                        .and_then(|_| ui.settings.set_string("sync-username", &dav.username));
                    if let Err(error) = saved { ui.toast(&format!("Could not save sync settings: {error}")); }
                    *ui.remote.lock().unwrap() = Some(Arc::new(dav));
                    controller.running.set(false);
                    controller.request();
                }
                Err(error) => {
                    controller.status(Some("network-offline-symbolic"), "Sync is not connected. Click to connect a WebDAV server.");
                    ui.toast(&error);
                }
            }
        });
    }

    fn disconnect(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        self.generation.set(self.generation.get() + 1);
        self.running.set(false);
        if let Some(dav) = ui.remote.lock().unwrap().take() {
            let _ = Command::new("secret-tool").args(["clear", "application", "oxidance", "url", dav.url.as_str(), "username", &dav.username])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        let _ = ui.settings.set_string("sync-url", "");
        let _ = ui.settings.set_string("sync-username", "");
        self.status(Some("network-offline-symbolic"), "Sync is not connected. Click to connect a WebDAV server.");
        ui.toast("Disconnected from the sync server. Your library and songs are kept on this device.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs against a temporary local rclone WebDAV server.
    #[test]
    #[ignore = "requires rclone"]
    fn sync_merges_libraries_and_transfers_songs_between_devices() {
        let root = std::env::temp_dir().join(format!("oxidance-sync-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let served = root.join("server");
        std::fs::create_dir_all(&served).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut server = Command::new("rclone").args(["serve", "webdav", served.to_str().unwrap(),
            "--addr", &format!("127.0.0.1:{port}"), "--baseurl", "/oxidance", "--user", "test", "--pass", "secret"])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        // Entering only the host finds the oxidance folder, as on the real server.
        let mut connected = Err(String::new());
        for _ in 0..50 {
            connected = Dav::connect(&format!("http://127.0.0.1:{port}"), "test".into(), "secret".into());
            if connected.is_ok() { break; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let dav = connected.unwrap();
        let url = dav.url.clone();
        assert_eq!(url.path(), "/oxidance/");
        assert!(Dav::connect(url.as_str(), "test".into(), "wrong".into()).err().unwrap().contains("rejected"));
        let song = |id: &str| Song { video_id: id.into(), title: format!("Title {id}"), artist: Some("Artist".into()), album_art_url: None, artists: vec![], source_url: None };
        let (events, _updates) = async_channel::unbounded();
        let device = |name: &str| {
            let directory = root.join(name);
            std::fs::create_dir_all(directory.join("Music")).unwrap();
            directory
        };
        let first = device("first");
        let second = device("second");
        // First device has a downloaded song and a playlist.
        let mut library_one = Library::default();
        library_one.toggle_like(&song("one"));
        let playlist = library_one.create_playlist("Mix").unwrap();
        library_one.add_song(playlist, &song("one")).unwrap();
        std::fs::write(first.join("Music/Artist - Title one [one].webm"), "audio one").unwrap();
        std::fs::write(first.join("Music/Artist - Title one [one].cover"), "cover one").unwrap();
        assert_eq!(cycle(&dav, &library_one, &first.join("sync-base.json"), &first.join("Music"), &events).unwrap(), "Uploaded 1 song.");
        assert_eq!(std::fs::read(served.join("songs/one.webm")).unwrap(), b"audio one");
        assert_eq!(std::fs::read(served.join("songs/one.cover")).unwrap(), b"cover one");
        // Second device starts with its own like and receives the first device's library.
        let mut library_two = Library::default();
        library_two.toggle_like(&song("two"));
        let merged = cycle(&dav, &library_two, &second.join("sync-base.json"), &second.join("Music"), &events)
            .and_then(|_| load_base(&second.join("sync-base.json"), url.as_str())).unwrap().unwrap();
        assert!(merged.is_liked(&song("one")) && merged.is_liked(&song("two")));
        assert_eq!(merged.playlists.len(), 1);
        // The second device downloads the song from the server, without YouTube.
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let audio = downloads::download(&second.join("Music"), &song("one"), Some(&dav), cancelled).unwrap();
        assert_eq!(audio.file_name().unwrap(), "Artist - Title one [one].webm");
        assert_eq!(std::fs::read(&audio).unwrap(), b"audio one");
        assert_eq!(std::fs::read(audio.with_extension("cover")).unwrap(), b"cover one");
        // An unlike on the first device propagates to the second.
        let library_one = load_base(&first.join("sync-base.json"), url.as_str()).unwrap().unwrap();
        cycle(&dav, &library_one, &first.join("sync-base.json"), &first.join("Music"), &events).unwrap();
        let mut library_one = load_base(&first.join("sync-base.json"), url.as_str()).unwrap().unwrap();
        assert!(library_one.is_liked(&song("two")));
        library_one.toggle_like(&song("two"));
        cycle(&dav, &library_one, &first.join("sync-base.json"), &first.join("Music"), &events).unwrap();
        cycle(&dav, &merged, &second.join("sync-base.json"), &second.join("Music"), &events).unwrap();
        let final_two = load_base(&second.join("sync-base.json"), url.as_str()).unwrap().unwrap();
        assert!(!final_two.is_liked(&song("two")));
        // An emptied local library is treated as a fresh device, not a mass deletion.
        cycle(&dav, &Library::default(), &second.join("sync-base.json"), &second.join("Music"), &events).unwrap();
        assert!(load_base(&second.join("sync-base.json"), url.as_str()).unwrap().unwrap().is_liked(&song("one")));
        // A brand-new device has no data directory yet.
        let fresh = root.join("fresh/oxidance/sync-base.json");
        cycle(&dav, &Library::default(), &fresh, &root.join("fresh/Music"), &events).unwrap();
        assert!(load_base(&fresh, url.as_str()).unwrap().unwrap().is_liked(&song("one")));
        // The lock is released after each pass.
        let token = dav.lock(LIBRARY).unwrap();
        dav.unlock(LIBRARY, &token).unwrap();
        server.kill().unwrap();
        let _ = server.wait();
        std::fs::remove_dir_all(root).unwrap();
    }
}
