//! Adds songs from links to other sites, such as Bandcamp or SoundCloud.
//! yt-dlp detects the title, artist, and cover; the user fills in anything it
//! missed and can listen to a preview before adding the song. A YouTube video
//! with chapters, such as a full album, can be added as one song per chapter.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use super::*;

pub struct AddLink {
    ui: std::rc::Weak<Ui>,
    target: View,
    dialog: adw::Dialog,
    link: adw::EntryRow,
    find: gtk::Button,
    spinner: adw::Spinner,
    status: gtk::Label,
    details: gtk::Box,
    cover: gtk::Image,
    fields: adw::PreferencesGroup,
    title: adw::EntryRow,
    artist: adw::EntryRow,
    split: adw::SwitchRow,
    chapter_group: adw::PreferencesGroup,
    chapters: RefCell<Vec<ChapterRow>>,
    preview: gtk::Button,
    position: gtk::Scale,
    time: gtk::Label,
    add: gtk::Button,
    /// The detected song, before the user's changes to its details.
    song: RefCell<Option<Song>>,
    stream: RefCell<Option<playback::Stream>>,
    /// A cover image the user chose instead of the detected one.
    chosen_cover: RefCell<Option<PathBuf>>,
    chooser: RefCell<Option<gtk::FileChooserNative>>,
    request: Cell<u64>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    player: RefCell<Option<gst::Element>>,
    bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    /// Where to start the preview once it has loaded, for previewing a chapter.
    pending_seek: Cell<Option<f64>>,
}

/// A chapter of the detected video, which the user can choose to add as a song.
struct ChapterRow {
    chapter: playback::Chapter,
    check: gtk::CheckButton,
    entry: adw::EntryRow,
}

fn target_name(ui: &Ui, target: View) -> Option<String> {
    match target {
        View::Liked => Some("Liked songs".into()),
        View::Playlist(id) => ui.library.borrow().playlists.iter().find(|playlist| playlist.id == id).map(|playlist| playlist.name.clone()),
        _ => None,
    }
}

fn contains(library: &Library, target: View, song: &Song) -> bool {
    match target {
        View::Playlist(id) => library.playlists.iter().any(|playlist| playlist.id == id
            && playlist.songs.iter().any(|saved| saved.video_id == song.video_id)),
        _ => library.is_liked(song),
    }
}

/// Lists missing details in a sentence, such as "the artist and cover".
fn join_missing(missing: &[&str]) -> String {
    match missing {
        [] => String::new(),
        [only] => format!("the {only}"),
        [rest @ .., last] => format!("the {} and {last}", rest.join(", ")),
    }
}

/// Opens the dialog for adding a song from a link to the visible library collection.
pub fn present(ui: &Rc<Ui>) -> Option<Rc<AddLink>> {
    let target = ui.view.get();
    let name = target_name(ui, target)?;
    let dialog = adw::Dialog::builder().title("Add Song From Link")
        // Grow when the detected song appears, instead of hiding it below a scroll.
        .follows_content_size(true).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let page = padded_box(12, 18);
    page.set_size_request(440, -1);

    let link_group = adw::PreferencesGroup::builder()
        .description("Paste a link to a song on Bandcamp, SoundCloud, or another site, or to a full album on YouTube.").build();
    let link = adw::EntryRow::builder().title("Link").build();
    link.set_input_purpose(gtk::InputPurpose::Url);
    let find = gtk::Button::builder().label("Find").valign(gtk::Align::Center).css_classes(["flat"]).build();
    link.add_suffix(&find);
    link_group.add(&link);
    page.append(&link_group);

    let progress = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spinner = adw::Spinner::builder().visible(false).build();
    let status = gtk::Label::builder().xalign(0.0).wrap(true).hexpand(true).visible(false).build();
    progress.append(&spinner);
    progress.append(&status);
    // Take no space, including the page's spacing, when there is nothing to report.
    status.bind_property("visible", &progress, "visible").sync_create().build();
    page.append(&progress);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 12);
    details.set_visible(false);
    let summary = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let cover = gtk::Image::from_icon_name("audio-x-generic-symbolic");
    cover.set_pixel_size(104);
    cover.set_size_request(104, 104);
    cover.set_valign(gtk::Align::Start);
    cover.add_css_class("card");
    summary.append(&cover);
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 4);
    controls.set_hexpand(true);
    controls.set_valign(gtk::Align::Center);
    let preview = gtk::Button::builder().halign(gtk::Align::Start).css_classes(["pill"]).build();
    let timeline = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let position = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    position.set_draw_value(false);
    position.set_hexpand(true);
    position.set_sensitive(false);
    position.set_tooltip_text(Some("Seek through preview"));
    let time = gtk::Label::builder().label("0:00").css_classes(["dim-label", "numeric"]).build();
    timeline.append(&position);
    timeline.append(&time);
    let choose = gtk::Button::builder().label("Choose Cover…").halign(gtk::Align::Start).css_classes(["flat"]).build();
    controls.append(&preview);
    controls.append(&timeline);
    controls.append(&choose);
    summary.append(&controls);
    details.append(&summary);
    let fields = adw::PreferencesGroup::new();
    let title = adw::EntryRow::builder().title("Title").build();
    let artist = adw::EntryRow::builder().title("Artist").build();
    let split = adw::SwitchRow::builder().title("Split Into Songs").visible(false).build();
    fields.add(&title);
    fields.add(&artist);
    fields.add(&split);
    details.append(&fields);
    let chapter_group = adw::PreferencesGroup::builder().title("Songs").visible(false).build();
    details.append(&chapter_group);
    page.append(&details);

    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&adw::Clamp::builder().maximum_size(480).tightening_threshold(480).child(&page).build()).build();
    toolbar.set_content(Some(&scroll));
    let add = gtk::Button::builder().label(format!("Add to {name}")).halign(gtk::Align::Center)
        .margin_top(12).margin_bottom(12).css_classes(["pill", "suggested-action"]).sensitive(false).build();
    toolbar.add_bottom_bar(&add);
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&link));

    let this = Rc::new(AddLink {
        ui: Rc::downgrade(ui), target, dialog, link, find, spinner, status, details, cover, fields, title, artist,
        split, chapter_group, chapters: RefCell::new(vec![]), preview, position, time, add,
        song: RefCell::new(None), stream: RefCell::new(None), chosen_cover: RefCell::new(None), chooser: RefCell::new(None),
        request: Cell::new(0), cancel: RefCell::new(None), player: RefCell::new(None), bus_watch: RefCell::new(None),
        pending_seek: Cell::new(None),
    });
    this.show_preview_state(false);
    let weak = Rc::downgrade(&this);
    this.find.connect_clicked({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.find(); } }});
    this.link.connect_entry_activated({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.find(); } }});
    for row in [&this.title, &this.artist] {
        row.connect_changed({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.update_details(); } }});
    }
    this.split.connect_active_notify({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.update_details(); } }});
    this.preview.connect_clicked({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.toggle_preview(); } }});
    this.position.connect_change_value({ let weak = weak.clone(); move |_, _, seconds| {
        if let Some(this) = weak.upgrade() && let Some(player) = this.player.borrow().as_ref() {
            let _ = player.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, gst::ClockTime::from_seconds_f64(seconds.max(0.0)));
        }
        glib::Propagation::Proceed
    }});
    choose.connect_clicked({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.choose_cover(); } }});
    this.add.connect_clicked({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.add(); } }});
    glib::timeout_add_local(Duration::from_millis(250), { let weak = weak.clone(); move || {
        let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break; };
        this.update_timeline();
        glib::ControlFlow::Continue
    }});
    // The closure keeps the dialog's state alive until it closes.
    this.dialog.connect_closed({ let this = RefCell::new(Some(this.clone())); move |_| {
        if let Some(this) = this.take() {
            this.cancel_request();
            this.stop_preview();
            if let Some(chooser) = this.chooser.take() { chooser.destroy(); }
        }
    }});
    this.dialog.present(Some(&ui.window));
    Some(this)
}

impl AddLink {
    fn set_status(&self, message: Option<&str>, error: bool) {
        self.status.set_visible(message.is_some());
        self.status.set_text(message.unwrap_or_default());
        if error { self.status.add_css_class("error"); self.status.remove_css_class("dim-label"); }
        else { self.status.remove_css_class("error"); self.status.add_css_class("dim-label"); }
    }

    fn cancel_request(&self) {
        if let Some(cancel) = self.cancel.take() { cancel.store(true, Ordering::Relaxed); }
    }

    fn find(self: &Rc<Self>) {
        let url = self.link.text().trim().to_owned();
        if url.is_empty() { self.link.grab_focus(); return; }
        self.cancel_request();
        self.stop_preview();
        self.request.set(self.request.get() + 1);
        let request = self.request.get();
        self.song.take();
        self.stream.take();
        self.chosen_cover.take();
        self.show_chapters(vec![]);
        self.details.set_visible(false);
        self.add.set_sensitive(false);
        self.spinner.set_visible(true);
        self.set_status(Some("Looking up the link…"), false);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel.borrow_mut() = Some(cancel.clone());
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || { let _ = sender.send_blocking(playback::detect(&url, cancel)); });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return; };
            let Some(this) = weak.upgrade() else { return; };
            if this.request.get() != request { return; }
            this.cancel.take();
            this.spinner.set_visible(false);
            match result {
                Ok(detected) => this.show(detected.song, detected.stream, detected.chapters, request),
                Err(error) => this.set_status(Some(&error), true),
            }
        });
    }

    fn show(self: &Rc<Self>, song: Song, stream: playback::Stream, chapters: Vec<playback::Chapter>, request: u64) {
        self.set_status(None, false);
        self.title.set_text(&song.title);
        self.artist.set_text(song.artist.as_deref().unwrap_or_default());
        self.cover.set_icon_name(Some("audio-x-generic-symbolic"));
        let art = song.album_art_url.clone();
        *self.song.borrow_mut() = Some(song);
        *self.stream.borrow_mut() = Some(stream);
        self.show_chapters(chapters);
        self.show_preview_state(false);
        self.details.set_visible(true);
        self.update_details();
        let Some(url) = art else { return; };
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build().map_err(|error| error.to_string())
                .and_then(|client| oxidance::fetch_image(&client, &url));
            let _ = sender.send_blocking(result);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return; };
            let Some(this) = weak.upgrade() else { return; };
            if this.request.get() != request || this.chosen_cover.borrow().is_some() { return; }
            match result.ok().and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok()) {
                Some(texture) => this.cover.set_paintable(Some(&texture)),
                // An unusable cover counts as missing, so the user is asked for one.
                None => if let Some(song) = this.song.borrow_mut().as_mut() { song.album_art_url = None; },
            }
            this.update_details();
        });
    }

    /// Lists the video's chapters, each checked to be added as a song.
    fn show_chapters(self: &Rc<Self>, chapters: Vec<playback::Chapter>) {
        for row in self.chapters.take() { self.chapter_group.remove(&row.entry); }
        self.split.set_visible(!chapters.is_empty());
        self.split.set_subtitle(&format!("Add each of the video’s {} chapters as its own song", chapters.len()));
        self.split.set_active(!chapters.is_empty());
        let weak = Rc::downgrade(self);
        for (index, chapter) in chapters.into_iter().enumerate() {
            let length = format_time((chapter.end - chapter.start).round() as u64);
            let entry = adw::EntryRow::builder().title(format!("{} · {length}", index + 1)).text(&chapter.title).build();
            let check = gtk::CheckButton::builder().active(true).valign(gtk::Align::Center).tooltip_text("Add this song").build();
            entry.add_prefix(&check);
            let play = gtk::Button::builder().icon_name("media-playback-start-symbolic").valign(gtk::Align::Center)
                .tooltip_text("Preview this song").css_classes(["flat", "circular"]).build();
            entry.add_suffix(&play);
            check.connect_toggled({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.update_details(); } }});
            entry.connect_changed({ let weak = weak.clone(); move |_| { if let Some(this) = weak.upgrade() { this.update_details(); } }});
            play.connect_clicked({ let weak = weak.clone(); let start = chapter.start; move |_| {
                if let Some(this) = weak.upgrade() { this.preview_from(start); }
            }});
            self.chapter_group.add(&entry);
            self.chapters.borrow_mut().push(ChapterRow { chapter, check, entry });
        }
    }

    fn splitting(&self) -> bool { self.split.is_visible() && self.split.is_active() }

    /// The song for one chapter of the detected video.
    fn chapter_song(&self, video: &Song, row: &ChapterRow) -> Song {
        let playback::Chapter { start, end, .. } = row.chapter;
        Song {
            video_id: format!("{}-t{}", video.video_id, start.round() as u64),
            title: row.entry.text().trim().to_owned(),
            artist: Some(self.artist.text().trim().to_owned()).filter(|artist| !artist.is_empty()),
            album_art_url: video.album_art_url.clone(), artists: vec![],
            source_url: Some(format!("https://www.youtube.com/watch?v={}", video.video_id)),
            section: Some(oxidance::Section { start, end }),
        }
    }

    /// Asks for missing details, and allows adding once the song has a title.
    fn update_details(&self) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let song = self.song.borrow();
        let Some(song) = song.as_ref() else { return; };
        if self.splitting() { return self.update_chapters(&ui, song); }
        self.title.set_visible(true);
        self.chapter_group.set_visible(false);
        self.add.set_label(&format!("Add to {}", target_name(&ui, self.target).unwrap_or_default()));
        let mut missing = Vec::new();
        if self.title.text().trim().is_empty() { missing.push("title"); }
        if self.artist.text().trim().is_empty() { missing.push("artist"); }
        if song.album_art_url.is_none() && self.chosen_cover.borrow().is_none() { missing.push("cover"); }
        for (row, name) in [(&self.title, "title"), (&self.artist, "artist")] {
            if missing.contains(&name) { row.add_css_class("warning"); } else { row.remove_css_class("warning"); }
        }
        self.fields.set_description(Some(&if missing.is_empty() {
            "Play the preview to check that this is the right song.".to_owned()
        } else {
            format!("Couldn’t detect {}. Add {} below.", join_missing(&missing), if missing.len() == 1 { "it" } else { "them" })
        }));
        let duplicate = contains(&ui.library.borrow(), self.target, song);
        if duplicate {
            let name = target_name(&ui, self.target).unwrap_or_else(|| "this playlist".into());
            self.set_status(Some(&format!("This song is already in {name}.")), true);
        }
        self.add.set_sensitive(ui.writable && !duplicate && !self.title.text().trim().is_empty());
    }

    /// Like `update_details`, for adding the checked chapters as songs.
    fn update_chapters(&self, ui: &Ui, video: &Song) {
        self.title.set_visible(false);
        self.chapter_group.set_visible(true);
        let name = target_name(ui, self.target).unwrap_or_default();
        let mut missing = Vec::new();
        if self.artist.text().trim().is_empty() { missing.push("artist"); }
        if video.album_art_url.is_none() && self.chosen_cover.borrow().is_none() { missing.push("cover"); }
        if missing.contains(&"artist") { self.artist.add_css_class("warning"); } else { self.artist.remove_css_class("warning"); }
        self.title.remove_css_class("warning");
        self.fields.set_description(Some(&if missing.is_empty() {
            "Every song uses this artist and cover.".to_owned()
        } else {
            format!("Couldn’t detect {}. Add {} below.", join_missing(&missing), if missing.len() == 1 { "it" } else { "them" })
        }));
        let (mut chosen, mut untitled, mut added) = (0, false, 0);
        for row in self.chapters.borrow().iter() {
            let duplicate = contains(&ui.library.borrow(), self.target, &self.chapter_song(video, row));
            row.check.set_sensitive(!duplicate);
            row.entry.set_tooltip_text(duplicate.then(|| format!("Already in {name}")).as_deref());
            if duplicate { added += 1; }
            let selected = row.check.is_active() && !duplicate;
            let empty = row.entry.text().trim().is_empty();
            if selected && empty { row.entry.add_css_class("warning"); } else { row.entry.remove_css_class("warning"); }
            if selected { chosen += 1; untitled |= empty; }
        }
        let total = self.chapters.borrow().len();
        self.chapter_group.set_description(Some(&match added {
            0 => "Uncheck any songs you don’t want. Each is saved as its own file.".to_owned(),
            added if added == total => format!("These songs are already in {name}."),
            added => format!("{added} of these songs are already in {name}."),
        }));
        self.add.set_label(&format!("Add {chosen} {} to {name}", if chosen == 1 { "Song" } else { "Songs" }));
        self.add.set_sensitive(ui.writable && chosen > 0 && !untitled);
    }

    fn choose_cover(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let chooser = gtk::FileChooserNative::new(Some("Choose Cover"), Some(&ui.window), gtk::FileChooserAction::Open, Some("Choose"), Some("Cancel"));
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_pixbuf_formats();
        chooser.add_filter(&filter);
        chooser.set_modal(true);
        chooser.connect_response({ let weak = Rc::downgrade(self); move |chooser, response| {
            let Some(this) = weak.upgrade() else { return; };
            if let Some(chooser) = this.chooser.take() { chooser.destroy(); }
            if response != gtk::ResponseType::Accept { return; }
            let Some(path) = chooser.file().and_then(|file| file.path()) else { return; };
            match gdk::Texture::from_filename(&path) {
                Ok(texture) => {
                    this.cover.set_paintable(Some(&texture));
                    *this.chosen_cover.borrow_mut() = Some(path);
                    this.update_details();
                }
                Err(error) => this.set_status(Some(&format!("Could not open that image: {error}")), true),
            }
        }});
        chooser.show();
        if let Some(previous) = self.chooser.replace(Some(chooser)) { previous.destroy(); }
    }

    fn show_preview_state(&self, playing: bool) {
        self.preview.set_child(Some(&adw::ButtonContent::builder()
            .icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" })
            .label(if playing { "Pause Preview" } else { "Play Preview" }).build()));
    }

    /// Plays the preview from `seconds` into the video, such as the start of a chapter.
    fn preview_from(self: &Rc<Self>, seconds: f64) {
        let player = self.player.borrow().clone();
        let Some(player) = player else {
            self.pending_seek.set(Some(seconds));
            return self.toggle_preview();
        };
        let _ = player.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::ClockTime::from_seconds_f64(seconds));
        let _ = player.set_state(gst::State::Playing);
        self.show_preview_state(true);
        self.pause_library_playback();
    }

    fn stop_preview(&self) {
        self.pending_seek.take();
        self.bus_watch.take();
        if let Some(player) = self.player.take() { let _ = player.set_state(gst::State::Null); }
        self.position.set_sensitive(false);
        self.position.set_value(0.0);
        self.time.set_text("0:00");
        self.show_preview_state(false);
    }

    fn toggle_preview(self: &Rc<Self>) {
        if let Some(player) = self.player.borrow().as_ref() {
            let playing = player.current_state() == gst::State::Playing || player.pending_state() == gst::State::Playing;
            let _ = player.set_state(if playing { gst::State::Paused } else { gst::State::Playing });
            self.show_preview_state(!playing);
            if !playing { self.pause_library_playback(); }
            return;
        }
        let Some(stream) = self.stream.borrow().clone() else { return; };
        match self.start_preview(stream) {
            Ok(()) => { self.show_preview_state(true); self.pause_library_playback(); }
            Err(error) => self.set_status(Some(&format!("Preview failed: {error}")), true),
        }
    }

    /// Only one thing plays at a time.
    fn pause_library_playback(&self) {
        if let Some(ui) = self.ui.upgrade() && ui.desired_playing.get() { ui.toggle_playback(); }
    }

    fn start_preview(self: &Rc<Self>, stream: playback::Stream) -> Result<(), String> {
        gst::init().map_err(|error| error.to_string())?;
        let player = gst::ElementFactory::make("playbin").build().map_err(|error| error.to_string())?;
        player.set_property_from_str("flags", "audio");
        player.set_property("uri", &stream.url);
        #[cfg(test)]
        player.set_property("audio-sink", gst::ElementFactory::make("fakesink").property("sync", true).build().map_err(|error| error.to_string())?);
        playback::send_headers(&player, stream.http_headers);
        let weak = Rc::downgrade(self);
        let guard = player.bus().ok_or("Playback bus unavailable")?.add_watch_local(move |_, message| {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break; };
            match message.view() {
                gst::MessageView::Error(error) => {
                    this.stop_preview();
                    this.set_status(Some(&format!("Preview failed: {}. Press Find to try again.", error.error())), true);
                    return glib::ControlFlow::Break;
                }
                gst::MessageView::Eos(_) => {
                    this.stop_preview();
                    return glib::ControlFlow::Break;
                }
                gst::MessageView::AsyncDone(_) => if let Some(seconds) = this.pending_seek.take() && let Some(player) = this.player.borrow().as_ref() {
                    let _ = player.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::ClockTime::from_seconds_f64(seconds));
                }
                _ => {}
            }
            glib::ControlFlow::Continue
        }).map_err(|error| error.to_string())?;
        *self.bus_watch.borrow_mut() = Some(guard);
        *self.player.borrow_mut() = Some(player.clone());
        player.set_state(gst::State::Playing).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn update_timeline(&self) {
        let player = self.player.borrow();
        let Some(player) = player.as_ref() else { return; };
        let (Some(duration), Some(position)) = (player.query_duration::<gst::ClockTime>(), player.query_position::<gst::ClockTime>()) else { return; };
        if duration.is_zero() { return; }
        self.position.set_range(0.0, duration.seconds_f64());
        self.position.set_value(position.seconds_f64());
        self.position.set_sensitive(true);
        self.time.set_text(&format!("{} / {}", format_time(position.seconds()), format_time(duration.seconds())));
    }

    fn add(self: &Rc<Self>) {
        if self.splitting() { return self.add_chapters(); }
        let Some(ui) = self.ui.upgrade() else { return; };
        let Some(mut song) = self.song.borrow().clone() else { return; };
        song.title = self.title.text().trim().to_owned();
        song.artist = Some(self.artist.text().trim().to_owned()).filter(|artist| !artist.is_empty());
        if let Some(path) = self.chosen_cover.borrow().as_ref() {
            match copy_cover(&ui.path, &song.video_id, path) {
                Ok(url) => song.album_art_url = Some(url),
                Err(error) => { self.set_status(Some(&format!("Could not save the cover: {error}")), true); return; }
            }
        }
        let target = self.target;
        let added = ui.change(|library| match target {
            View::Playlist(id) => library.add_song(id, &song)?.then_some(()).ok_or_else(|| "This song is already in the playlist.".into()),
            _ => library.like(&song).then_some(()).ok_or_else(|| "This song is already in Liked songs.".into()),
        });
        if added {
            ui.toast(&format!("Added “{}”", song.title));
            self.dialog.close();
        }
    }
}

impl AddLink {
    fn add_chapters(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return; };
        let Some(mut video) = self.song.borrow().clone() else { return; };
        if let Some(path) = self.chosen_cover.borrow().as_ref() {
            match copy_cover(&ui.path, &video.video_id, path) {
                Ok(url) => video.album_art_url = Some(url),
                Err(error) => { self.set_status(Some(&format!("Could not save the cover: {error}")), true); return; }
            }
        }
        let songs: Vec<Song> = self.chapters.borrow().iter().filter(|row| row.check.is_active() && row.check.is_sensitive())
            .map(|row| self.chapter_song(&video, row)).collect();
        let target = self.target;
        let added = ui.change(|library| {
            match target {
                View::Playlist(id) => for song in &songs { library.add_song(id, song)?; },
                // Liking puts a song first, so like the last track first to keep the album's order.
                _ => for song in songs.iter().rev() { library.like(song); },
            }
            Ok(())
        });
        if added {
            ui.toast(&format!("Added {} {}", songs.len(), if songs.len() == 1 { "song" } else { "songs" }));
            self.dialog.close();
        }
    }
}

/// Keeps a chosen cover with the library, since the original file may move.
/// The name changes with each choice so cached images of an older cover are not reused.
fn copy_cover(library: &std::path::Path, id: &str, source: &std::path::Path) -> Result<String, String> {
    let directory = library.parent().ok_or("Missing library directory")?.join("covers");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let extension = source.extension().and_then(|extension| extension.to_str()).unwrap_or("image");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let path = directory.join(format!("{id}-{stamp}.{extension}"));
    std::fs::copy(source, &path).map_err(|error| error.to_string())?;
    Ok(gtk::gio::File::for_path(path).uri().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump_for(duration: Duration) {
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            while context.pending() { context.iteration(false); }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            if done() { return true; }
            pump_for(Duration::from_millis(50));
        }
        done()
    }

    #[test]
    fn missing_details_are_listed_in_a_sentence() {
        assert_eq!(join_missing(&["cover"]), "the cover");
        assert_eq!(join_missing(&["artist", "cover"]), "the artist and cover");
        assert_eq!(join_missing(&["title", "artist", "cover"]), "the title, artist and cover");
    }

    #[test]
    #[ignore = "requires a desktop display and network access for yt-dlp"]
    fn ui_add_bandcamp_song_with_preview_and_custom_cover() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.AddLinkTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-add-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let ui = Ui::new(&app, directory.join("library.json"));
        ui.navigate(View::Liked);
        let this = present(&ui).unwrap();
        this.link.set_text("https://grantnelsonmusic.bandcamp.com/track/anytime-anywhere-piano-pressure-mix");
        this.find();
        assert!(wait_until(60, || this.details.is_visible() || this.status.has_css_class("error")), "lookup did not finish");
        assert!(this.details.is_visible(), "lookup failed: {}", this.status.text());
        assert_eq!(this.title.text(), "Anytime, Anywhere (Piano Pressure Mix)");
        assert_eq!(this.artist.text(), "Livin' Large");
        assert!(wait_until(20, || this.cover.paintable().is_some()), "detected cover must load");
        assert!(this.add.is_sensitive());

        this.toggle_preview();
        assert!(wait_until(30, || this.player.borrow().as_ref()
            .and_then(|player| player.query_position::<gst::ClockTime>()).is_some_and(|position| position > gst::ClockTime::from_mseconds(500))),
            "preview must play: {}", this.status.text());
        this.toggle_preview();

        // A missing artist is requested, and a chosen cover replaces the detected one.
        this.artist.set_text("");
        assert!(this.fields.description().unwrap().contains("the artist"));
        assert!(this.title.text().len() > 0 && this.add.is_sensitive(), "the artist is optional");
        this.artist.set_text("Livin' Large");
        std::fs::create_dir_all(&directory).unwrap();
        let image = directory.join("chosen.png");
        gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 4, 4).unwrap().savev(&image, "png", &[]).unwrap();
        *this.chosen_cover.borrow_mut() = Some(image);
        this.add();
        pump_for(Duration::from_millis(100));
        let song = ui.library.borrow().liked[0].clone();
        assert_eq!(song.video_id, "Bandcamp-2707922679");
        assert_eq!(song.page_url(), "https://grantnelsonmusic.bandcamp.com/track/anytime-anywhere-piano-pressure-mix");
        assert!(song.album_art_url.as_deref().unwrap().starts_with("file://"));
        assert!(this.player.borrow().is_none(), "closing the dialog stops the preview");

        // Adding the same song again is refused.
        let again = present(&ui).unwrap();
        *again.song.borrow_mut() = Some(song.clone());
        again.title.set_text(&song.title);
        again.update_details();
        assert!(!again.add.is_sensitive());
        again.dialog.close();

        // The song downloads from its own page and keeps the chosen cover.
        let queue = downloads::Queue::new(ui.music_directory.clone(), Default::default()).0;
        queue.enqueue(&song);
        assert!(wait_until(120, || !queue.is_pending(&song.video_id)), "download did not finish");
        let audio = downloads::local_file(&ui.music_directory, &song.video_id).expect("song must download");
        assert!(audio.to_string_lossy().ends_with("[Bandcamp-2707922679].mp3"), "{audio:?}");
        let art = gtk::gdk_pixbuf::Pixbuf::from_file(downloads::local_art(&ui.music_directory, &song.video_id).unwrap()).unwrap();
        assert_eq!((art.width(), art.height()), (4, 4));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display and network access for yt-dlp"]
    fn ui_add_chosen_chapters_of_full_album_video() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.AddChaptersTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-add-chapters-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let ui = Ui::new(&app, directory.join("library.json"));
        ui.start_download_queue();
        ui.navigate(View::Liked);
        let this = present(&ui).unwrap();
        this.link.set_text("https://youtu.be/1Jp6ne59-4U");
        this.find();
        assert!(wait_until(60, || this.details.is_visible() || this.status.has_css_class("error")), "lookup did not finish");
        assert!(this.details.is_visible(), "lookup failed: {}", this.status.text());
        assert!(this.splitting() && !this.title.is_visible());
        assert_eq!(this.artist.text(), "CECIL");
        let titles: Vec<_> = this.chapters.borrow().iter().map(|row| row.entry.text().to_string()).collect();
        assert_eq!(titles.len(), 6);
        assert_eq!(titles[1], "シトロエン (Citroen)");
        assert_eq!(this.add.label().unwrap(), "Add 6 Songs to Liked songs");

        // Previewing a chapter starts the video at that chapter.
        this.preview_from(120.0);
        assert!(wait_until(30, || this.player.borrow().as_ref()
            .and_then(|player| player.query_position::<gst::ClockTime>()).is_some_and(|position| position >= gst::ClockTime::from_seconds(120))),
            "chapter preview must start at the chapter: {}", this.status.text());
        this.toggle_preview();

        // Only the checked songs are added, renamed as edited, in the album's order.
        for (index, row) in this.chapters.borrow().iter().enumerate() { row.check.set_active(index == 0 || index == 1); }
        this.chapters.borrow()[0].entry.set_text("");
        assert!(!this.add.is_sensitive(), "a checked song needs a title");
        this.chapters.borrow()[0].entry.set_text("Summer Clock");
        assert_eq!(this.add.label().unwrap(), "Add 2 Songs to Liked songs");
        this.add();
        pump_for(Duration::from_millis(100));
        let liked = ui.library.borrow().liked.clone();
        let titles: Vec<_> = liked.iter().map(|song| song.title.as_str()).collect();
        assert_eq!(titles, ["Summer Clock", "シトロエン (Citroen)"]);
        assert_eq!(liked[1].video_id, "1Jp6ne59-4U-t120");
        assert_eq!(liked[1].section, Some(oxidance::Section { start: 120.0, end: 331.0 }));
        assert_eq!(liked[1].artist.as_deref(), Some("CECIL"));

        // Adding again offers only the songs not yet added.
        let again = present(&ui).unwrap();
        again.link.set_text("https://youtu.be/1Jp6ne59-4U");
        again.find();
        assert!(wait_until(60, || again.details.is_visible()), "second lookup failed: {}", again.status.text());
        assert!(!again.chapters.borrow()[1].check.is_sensitive());
        assert_eq!(again.add.label().unwrap(), "Add 4 Songs to Liked songs");
        again.dialog.close();

        // Adding saves each song, cut from one download of the video at its chapter.
        assert!(wait_until(180, || liked.iter().all(|song| downloads::local_file(&ui.music_directory, &song.video_id).is_some())),
            "downloads did not finish");
        for (song, seconds) in liked.iter().zip([120.0, 211.0]) {
            let audio = downloads::local_file(&ui.music_directory, &song.video_id).expect("song must be saved");
            let output = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
                .arg(&audio).output().unwrap();
            let duration: f64 = String::from_utf8_lossy(&output.stdout).trim().parse().unwrap();
            assert!((duration - seconds).abs() < 0.5, "{audio:?} lasts {duration}s, not {seconds}s");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
