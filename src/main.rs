use std::{cell::{Cell, RefCell}, collections::HashMap, path::PathBuf, rc::Rc, time::Duration};

use adw::prelude::*;
use gtk::{gdk, glib};
use gst::prelude::*;
use oxidance::{Song, library::Library};
mod playback;
mod downloads;
mod settings;
mod mpris;
mod dav;
mod sync;

#[derive(Clone, Copy, PartialEq)]
enum View { Search, Liked, Playlist(u64), Artist }

struct Ui {
    window: adw::ApplicationWindow,
    split: adw::OverlaySplitView,
    sidebar: gtk::Box,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    retry: gtk::Button,
    entry: gtk::Entry,
    collection_search: gtk::Entry,
    search_controls: gtk::Stack,
    title: gtk::Label,
    status: gtk::Label,
    spinner: gtk::Spinner,
    toasts: adw::ToastOverlay,
    library: RefCell<Library>,
    path: PathBuf,
    writable: bool,
    view: Cell<View>,
    results: RefCell<Vec<Song>>,
    artist_results: RefCell<Vec<oxidance::artists::Artist>>,
    artist_profile: RefCell<Option<oxidance::artists::Profile>>,
    artist_request: Cell<u64>,
    artist_loading: Cell<bool>,
    biography_expanded: Cell<bool>,
    artist_error: RefCell<Option<String>>,
    artist_back: gtk::Button,
    artist_previous: Cell<View>,
    artist_previous_scroll: Cell<f64>,
    artist_previous_filter: RefCell<String>,
    generation: Cell<u64>,
    search_request: Cell<u64>,
    debounce: RefCell<Option<glib::SourceId>>,
    query: RefCell<String>,
    cursor: RefCell<Option<oxidance::SearchCursor>>,
    page_loading: Cell<bool>,
    pagination_failed: Cell<bool>,
    art_cache: RefCell<HashMap<String, gdk::Texture>>,
    pending_art: RefCell<Vec<(gtk::Image, String)>>,
    playback_bar: gtk::Box,
    playback_art: gtk::Image,
    sidebar_art: gtk::Picture,
    playback_button: gtk::Button,
    previous_button: gtk::Button,
    next_button: gtk::Button,
    shuffle_button: gtk::ToggleButton,
    playback_queue: RefCell<Vec<Song>>,
    playback_order: RefCell<Vec<usize>>,
    queue_position: Cell<usize>,
    volume: Cell<f64>,
    mpris: RefCell<Option<mpris::Service>>,
    playback_title: gtk::Label,
    seek: gtk::Scale,
    elapsed: gtk::Label,
    duration: gtk::Label,
    playback_rate: Cell<f64>,
    settings: gtk::gio::Settings,
    applied_rate: Cell<f64>,
    seek_pending: Cell<bool>,
    seek_dragging: Cell<bool>,
    player: RefCell<Option<gst::Element>>,
    bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    current_song: RefCell<Option<Song>>,
    desired_playing: Cell<bool>,
    resolving: Cell<bool>,
    buffering: Cell<bool>,
    playback_generation: Cell<u64>,
    resolve_cancel: RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
    row_play_buttons: RefCell<Vec<(String, glib::WeakRef<gtk::Button>)>>,
    music_directory: PathBuf,
    download_queue: RefCell<Option<downloads::Queue>>,
    download_spinners: RefCell<Vec<(String, glib::WeakRef<gtk::Stack>)>>,
    completed_downloads: RefCell<HashMap<String, std::time::Instant>>,
    remote: downloads::Remote,
    sync: RefCell<Option<Rc<sync::Controller>>>,
}

fn padded_box(spacing: i32, margin: i32) -> gtk::Box {
    let widget = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    widget.set_margin_top(margin);
    widget.set_margin_bottom(margin);
    widget.set_margin_start(margin);
    widget.set_margin_end(margin);
    widget
}

fn format_time(seconds: u64) -> String {
    if seconds >= 3600 { format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60) }
    else { format!("{}:{:02}", seconds / 60, seconds % 60) }
}

impl Ui {
    fn new(app: &adw::Application, path: PathBuf) -> Rc<Self> {
        let (library, load_error) = match Library::load(&path) {
            Ok(library) => (library, None),
            Err(error) => (Library::default(), Some(error.to_string())),
        };
        let window = adw::ApplicationWindow::builder().application(app)
            .title("Oxidance").default_width(920).default_height(620).build();
        let split = adw::OverlaySplitView::new();
        split.set_min_sidebar_width(220.0);
        split.set_max_sidebar_width(280.0);
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let options_model = gtk::gio::Menu::new();
        options_model.append(Some("Preferences"), Some("win.preferences"));
        let options = gtk::MenuButton::builder().icon_name("open-menu-symbolic").tooltip_text("Options")
            .menu_model(&options_model).build();
        header.pack_end(&options);
        let sync_button = gtk::Button::builder().css_classes(["flat"]).build();
        header.pack_end(&sync_button);
        let header_title = gtk::Stack::new();
        header_title.add_named(&adw::WindowTitle::new("Oxidance", ""), Some("title"));
        header.set_title_widget(Some(&header_title));
        let search_page = gtk::Button::builder().icon_name("system-search-symbolic")
            .tooltip_text("Search songs").build();
        let toggle = gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic")
            .tooltip_text("Show or hide library sidebar").active(true).build();
        header.pack_start(&toggle);
        header.pack_start(&search_page);
        let artist_back = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Back").visible(false).build();
        header.pack_start(&artist_back);
        split.bind_property("show-sidebar", &toggle, "active").bidirectional().sync_create().build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Playlists", "")));
        let sidebar_toggle = gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic")
            .tooltip_text("Show or hide library sidebar").active(true).build();
        sidebar_header.pack_start(&sidebar_toggle);
        split.bind_property("show-sidebar", &sidebar_toggle, "active").bidirectional().sync_create().build();
        let update_toggle = {
            let toggle = toggle.clone();
            let sidebar_toggle = sidebar_toggle.clone();
            move |split: &adw::OverlaySplitView| {
                if split.shows_sidebar() && !split.is_collapsed() && toggle.has_focus() {
                    sidebar_toggle.grab_focus();
                }
                toggle.set_visible(!split.shows_sidebar() || split.is_collapsed());
            }
        };
        update_toggle(&split);
        split.connect_show_sidebar_notify(update_toggle.clone());
        split.connect_collapsed_notify(update_toggle);
        toolbar.add_top_bar(&header);
        let content = padded_box(16, 20);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let entry = gtk::Entry::builder().placeholder_text("Search songs or artists").hexpand(true).build();
        entry.set_width_chars(32);
        controls.append(&entry);
        header_title.add_named(&controls, Some("search"));
        header_title.set_visible_child_name("search");
        let title = gtk::Label::builder().label("Search results").xalign(0.0).build();
        title.add_css_class("title-2");
        content.append(&title);
        let progress = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        let status = gtk::Label::builder().label("Enter a query to find songs.")
            .xalign(0.0).wrap(true).hexpand(true).build();
        status.add_css_class("dim-label");
        progress.append(&spinner);
        progress.append(&status);
        let retry = gtk::Button::with_label("Retry");
        retry.set_visible(false);
        progress.append(&retry);
        content.append(&progress);
        let collection_search = gtk::Entry::builder().placeholder_text("Search this collection")
            .secondary_icon_name("edit-clear-symbolic").visible(false).build();
        content.append(&collection_search);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true).child(&list).build();
        content.append(&scroll);
        let playback_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        playback_bar.set_visible(false);
        let playback_art = gtk::Image::from_icon_name("audio-x-generic-symbolic");
        playback_art.set_pixel_size(72);
        playback_art.set_size_request(72, 72);
        playback_art.set_valign(gtk::Align::Center);
        playback_bar.append(&playback_art);
        let playback_button = gtk::Button::builder().icon_name("media-playback-start-symbolic")
            .tooltip_text("Play").valign(gtk::Align::Center).build();
        let previous_button = gtk::Button::builder().icon_name("media-skip-backward-symbolic").tooltip_text("Previous song").valign(gtk::Align::Center).build();
        let next_button = gtk::Button::builder().icon_name("media-skip-forward-symbolic").tooltip_text("Next song").valign(gtk::Align::Center).build();
        let shuffle_button = gtk::ToggleButton::builder().icon_name("media-playlist-shuffle-symbolic").tooltip_text("Shuffle").valign(gtk::Align::Center).build();
        let info = gtk::Box::new(gtk::Orientation::Vertical, 4);
        info.set_hexpand(true);
        info.set_valign(gtk::Align::Center);
        let playback_title = gtk::Label::builder().xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End).build();
        playback_title.add_css_class("heading");
        info.append(&playback_title);
        let timeline = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let elapsed = gtk::Label::new(Some("0:00"));
        let duration = gtk::Label::new(Some("—:—"));
        for label in [&elapsed, &duration] {
            label.set_width_chars(5);
            label.set_max_width_chars(5);
            let attributes = gtk::pango::AttrList::new();
            attributes.insert(gtk::pango::AttrFontFeatures::new("tnum=1"));
            label.set_attributes(Some(&attributes));
        }
        elapsed.set_xalign(0.0);
        duration.set_xalign(1.0);
        let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        seek.set_draw_value(false);
        seek.set_hexpand(true);
        seek.set_sensitive(false);
        seek.set_tooltip_text(Some("Seek through song"));
        timeline.append(&elapsed);
        timeline.append(&seek);
        timeline.append(&duration);
        let speed_button = gtk::MenuButton::builder().tooltip_text("Playback speed").valign(gtk::Align::Center).build();
        let speed_icon = gtk::DrawingArea::builder().content_width(20).content_height(20).build();
        speed_icon.set_draw_func(|widget, cr, width, height| {
            let color = widget.style_context().color();
            cr.set_source_rgba(color.red() as f64, color.green() as f64, color.blue() as f64, color.alpha() as f64);
            cr.scale(width as f64 / 20.0, height as f64 / 20.0);
            cr.set_line_width(1.8);
            cr.arc(10.0, 12.0, 8.0, std::f64::consts::PI, 2.0 * std::f64::consts::PI);
            cr.line_to(18.0, 15.0);
            cr.line_to(2.0, 15.0);
            cr.close_path();
            let _ = cr.stroke();
            cr.move_to(10.0, 12.0);
            cr.line_to(14.0, 7.0);
            let _ = cr.stroke();
        });
        speed_button.set_child(Some(&speed_icon));
        let speed_popover = gtk::Popover::new();
        let speed_controls = padded_box(8, 12);
        let settings = settings::load();
        let saved_speed = settings.double("playback-speed");
        let speed_label = gtk::Label::new(Some(&format!("{saved_speed:.2}×")));
        let speed_slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.5, 2.0, 0.01);
        speed_slider.set_digits(2);
        speed_slider.set_value(saved_speed);
        speed_slider.add_mark(1.0, gtk::PositionType::Bottom, None);
        let speed_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE);
        speed_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        speed_scroll.connect_scroll({ let slider = speed_slider.downgrade(); move |_, _, dy| {
            if let Some(slider) = slider.upgrade() {
                if dy != 0.0 { slider.set_value(((slider.value() - dy.signum() * 0.01) * 100.0).round() / 100.0); }
            }
            glib::Propagation::Stop
        }});
        speed_slider.add_controller(speed_scroll);
        speed_slider.set_draw_value(false);
        speed_slider.set_size_request(220, -1);
        speed_slider.set_tooltip_text(Some("Playback speed (changes pitch)"));
        speed_controls.append(&speed_label);
        speed_controls.append(&speed_slider);
        speed_popover.set_child(Some(&speed_controls));
        speed_button.set_popover(Some(&speed_popover));
        timeline.append(&speed_button);
        info.append(&timeline);
        let transport = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let skips = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        skips.append(&previous_button);
        skips.append(&playback_button);
        skips.append(&next_button);
        transport.append(&skips);
        transport.append(&shuffle_button);
        transport.set_valign(gtk::Align::Center);
        playback_bar.append(&transport);
        playback_bar.append(&info);
        content.append(&playback_bar);
        toolbar.set_content(Some(&content));
        split.set_content(Some(&toolbar));
        let sidebar = padded_box(8, 12);
        let sidebar_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true).child(&sidebar).build();
        let sidebar_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_content.append(&sidebar_scroll);
        let sidebar_art = gtk::Picture::new();
        sidebar_art.set_can_shrink(true);
        sidebar_art.set_keep_aspect_ratio(true);
        sidebar_art.set_visible(false);
        let sidebar_art_frame = gtk::AspectFrame::builder().ratio(1.0).obey_child(false)
            .hexpand(true).child(&sidebar_art).build();
        sidebar_art_frame.add_css_class("sidebar-album-art");
        let artwork_css = gtk::CssProvider::new();
        artwork_css.load_from_data(".sidebar-album-art { background-color: #000; }");
        gtk::style_context_add_provider_for_display(&gdk::Display::default().unwrap(),
            &artwork_css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        sidebar_art_frame.set_margin_end(1);
        sidebar_art.bind_property("visible", &sidebar_art_frame, "visible").sync_create().build();
        sidebar_content.append(&sidebar_art_frame);
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&sidebar_header);
        sidebar_toolbar.set_content(Some(&sidebar_content));
        split.set_sidebar(Some(&sidebar_toolbar));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));
        // On narrow windows the sidebar becomes a dismissible overlay.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 700sp").unwrap());
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);
        #[cfg(not(test))]
        let music_directory = glib::home_dir().join("Music");
        #[cfg(test)]
        let music_directory = path.parent().unwrap().join("Music");
        let ui = Rc::new(Self {
            window, split, sidebar, list, scroll, retry, entry, collection_search, search_controls: header_title, title, status, spinner,
            toasts, library: RefCell::new(library), path, writable: load_error.is_none(),
            view: Cell::new(View::Liked), results: RefCell::new(vec![]),
            artist_results: RefCell::new(vec![]), artist_profile: RefCell::new(None),
            artist_request: Cell::new(0), artist_loading: Cell::new(false), biography_expanded: Cell::new(false), artist_error: RefCell::new(None),
            artist_back, artist_previous: Cell::new(View::Search),
            artist_previous_scroll: Cell::new(0.0), artist_previous_filter: RefCell::new(String::new()),
            generation: Cell::new(0), search_request: Cell::new(0), art_cache: RefCell::new(HashMap::new()),
            debounce: RefCell::new(None), query: RefCell::new(String::new()),
            cursor: RefCell::new(None), page_loading: Cell::new(false), pagination_failed: Cell::new(false), pending_art: RefCell::new(vec![]),
            playback_bar, playback_art, sidebar_art, playback_button, playback_title,
            previous_button, next_button, shuffle_button, playback_queue: RefCell::new(vec![]),
            playback_order: RefCell::new(vec![]), queue_position: Cell::new(0), volume: Cell::new(1.0), mpris: RefCell::new(None),
            seek, elapsed, duration, seek_pending: Cell::new(false), seek_dragging: Cell::new(false),
            playback_rate: Cell::new(saved_speed), applied_rate: Cell::new(1.0), settings,
            player: RefCell::new(None), bus_watch: RefCell::new(None), current_song: RefCell::new(None),
            desired_playing: Cell::new(false), resolving: Cell::new(false), buffering: Cell::new(false),
            playback_generation: Cell::new(0), resolve_cancel: RefCell::new(None), row_play_buttons: RefCell::new(vec![]),
            music_directory, download_queue: RefCell::new(None), download_spinners: RefCell::new(vec![]), completed_downloads: RefCell::new(HashMap::new()),
            remote: Default::default(), sync: RefCell::new(None),
        });
        let create = ui.playlist_menu(None);
        create.set_tooltip_text(Some("Create playlist"));
        sidebar_header.pack_end(&create);
        let preferences = gtk::gio::SimpleAction::new("preferences", None);
        preferences.connect_activate({ let weak = Rc::downgrade(&ui); move |_, _| {
            if let Some(ui) = weak.upgrade() { ui.show_preferences(); }
        }});
        ui.window.add_action(&preferences);
        sync::install(&ui, &sync_button, &options_model);
        ui.settings.connect_changed(Some("artwork-in-sidebar"), { let weak = Rc::downgrade(&ui); move |_, _| {
            if let Some(ui) = weak.upgrade() { ui.update_artwork_placement(); }
        }});
        ui.split.connect_show_sidebar_notify({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.update_artwork_placement(); }
        }});
        ui.playback_art.connect_notify_local(None, { let weak = Rc::downgrade(&ui); move |_, property| {
            if matches!(property.name(), "paintable" | "icon-name") {
                if let Some(ui) = weak.upgrade() { ui.update_sidebar_art(); }
            }
        }});
        ui.update_artwork_placement();
        ui.previous_button.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| { if let Some(ui) = weak.upgrade() { ui.advance_queue(false); } }});
        ui.next_button.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| { if let Some(ui) = weak.upgrade() { ui.advance_queue(true); } }});
        ui.shuffle_button.connect_toggled({ let weak = Rc::downgrade(&ui); move |_| { if let Some(ui) = weak.upgrade() { ui.reorder_queue(); } }});
        ui.settings.bind("shuffle", &ui.shuffle_button, "active").build();
        ui.settings.connect_changed(Some("playback-speed"), { let slider = speed_slider.clone(); move |settings, _| { slider.set_value(settings.double("playback-speed")); } });
        ui.playback_button.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.toggle_playback(); }
        }});
        let playback_keys = gtk::EventControllerKey::new();
        playback_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        playback_keys.connect_key_pressed({ let weak = Rc::downgrade(&ui); move |_, key, _, modifiers| {
            let Some(ui) = weak.upgrade() else { return glib::Propagation::Proceed; };
            if key != gdk::Key::space || modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK)
                || ui.window.visible_dialog().is_some() { return glib::Propagation::Proceed; }
            let mut focus = gtk::prelude::GtkWindowExt::focus(&ui.window);
            while let Some(widget) = focus {
                if widget.is::<gtk::Editable>() || widget.is::<gtk::TextView>() { return glib::Propagation::Proceed; }
                focus = widget.parent();
            }
            if ui.current_song.borrow().is_none() { return glib::Propagation::Proceed; }
            ui.toggle_playback();
            glib::Propagation::Stop
        }});
        ui.window.add_controller(playback_keys);
        ui.collection_search.connect_changed({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() {
                if matches!(ui.view.get(), View::Liked | View::Playlist(_)) {
                    ui.render();
                    ui.scroll.vadjustment().set_value(0.0);
                }
            }
        }});
        ui.collection_search.connect_icon_release(|entry, position| {
            if position == gtk::EntryIconPosition::Secondary { entry.set_text(""); }
        });
        speed_slider.connect_value_changed({ let weak = Rc::downgrade(&ui); move |slider| {
            if let Some(ui) = weak.upgrade() {
                let rate = slider.value();
                speed_label.set_text(&format!("{rate:.2}×"));
                ui.playback_rate.set(rate);
                if let Err(error) = ui.settings.set_double("playback-speed", rate) {
                    ui.toast(&format!("Could not save playback speed: {error}"));
                }
                ui.update_timeline();
            }
        }});
        ui.seek.connect_change_value({ let weak = Rc::downgrade(&ui); move |_, _, seconds| {
            if let Some(ui) = weak.upgrade() { ui.seek_to(seconds); }
            glib::Propagation::Stop
        }});
        let drag = gtk::GestureClick::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        drag.connect_pressed({ let weak = Rc::downgrade(&ui); move |_, _, _, _| {
            if let Some(ui) = weak.upgrade() { ui.seek_dragging.set(true); }
        }});
        drag.connect_released({ let weak = Rc::downgrade(&ui); move |_, _, _, _| {
            if let Some(ui) = weak.upgrade() { ui.seek_dragging.set(false); }
        }});
        drag.connect_stopped({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.seek_dragging.set(false); }
        }});
        ui.seek.add_controller(drag);
        glib::timeout_add_local(Duration::from_millis(250), { let weak = Rc::downgrade(&ui); move || {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break; };
            ui.update_timeline();
            if let Some(service) = ui.mpris.borrow().as_ref() { service.update(&ui); }
            glib::ControlFlow::Continue
        }});
        search_page.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.navigate(View::Search);
                ui.entry.grab_focus();
            }
        }});
        ui.artist_back.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.return_from_artist(); }
        }});
        ui.entry.connect_activate({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.search(); }
        }});
        ui.entry.connect_changed({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.schedule_search(); }
        }});
        ui.scroll.vadjustment().connect_value_changed({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() { ui.load_visible_art(); ui.maybe_load_more(); }
        }});
        ui.scroll.vadjustment().connect_changed({ let weak = Rc::downgrade(&ui); move |_| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || { if let Some(ui) = weak.upgrade() { ui.load_visible_art(); ui.maybe_load_more(); } });
        }});
        ui.retry.connect_clicked({ let weak = Rc::downgrade(&ui); move |_| {
            if let Some(ui) = weak.upgrade() {
                ui.pagination_failed.set(false);
                if ui.results.borrow().is_empty() { ui.search(); } else { ui.load_more(); }
            }
        }});
        ui.refresh_sidebar();
        ui.render();
        ui.window.present();
        #[cfg(not(test))]
        ui.start_download_queue();
        #[cfg(not(test))]
        ui.restore_last_song();
        search_page.grab_focus();
        if let Some(error) = load_error {
            ui.status.set_text(&format!("Cannot load saved library: {error}. Library changes are disabled to protect the file."));
            ui.status.add_css_class("error");
        }
        ui
    }

    fn show_preferences(self: &Rc<Self>) {
        let dialog = adw::PreferencesDialog::builder().title("Preferences").build();
        let page = adw::PreferencesPage::builder().title("Appearance").icon_name("preferences-desktop-appearance-symbolic").build();
        let group = adw::PreferencesGroup::builder().title("Album art").build();
        let row = adw::SwitchRow::builder().title("Show album art in sidebar")
            .subtitle("Show the playing song’s artwork enlarged at the bottom of the sidebar.").build();
        self.settings.bind("artwork-in-sidebar", &row, "active").build();
        group.add(&row);
        page.add(&group);
        dialog.add(&page);
        dialog.present(Some(&self.window));
    }

    fn update_sidebar_art(&self) {
        if let Some(paintable) = self.playback_art.paintable() {
            self.sidebar_art.set_paintable(Some(&paintable));
        } else { self.sidebar_art.set_paintable(None::<&gdk::Paintable>); }
    }

    fn update_artwork_placement(&self) {
        let in_sidebar = self.settings.boolean("artwork-in-sidebar") && self.split.shows_sidebar();
        self.sidebar_art.set_visible(in_sidebar && self.current_song.borrow().is_some());
        self.playback_art.set_visible(!in_sidebar);
        self.update_sidebar_art();
    }

    fn toast(&self, message: &str) { self.toasts.add_toast(adw::Toast::new(message)); }

    // Commit the visible state only after the replacement file is safely written.
    fn change(self: &Rc<Self>, action: impl FnOnce(&mut Library) -> Result<(), String>) -> bool {
        if !self.writable { self.toast("Saved library could not be loaded; changes are disabled."); return false; }
        let previous = self.library.borrow().clone();
        let mut next = self.library.borrow().clone();
        if let Err(error) = action(&mut next) { self.toast(&error); return false; }
        if let Err(error) = next.save(&self.path) {
            self.toast(&format!("Could not save library: {error}"));
            return false;
        }
        *self.library.borrow_mut() = next;
        for song in self.saved_songs() {
            let newly_saved = {
                let library = self.library.borrow();
                (library.is_liked(&song) && !previous.is_liked(&song)) || library.playlists.iter().any(|playlist| {
                    playlist.songs.iter().any(|saved| saved.video_id == song.video_id)
                        && !previous.playlists.iter().any(|old| old.id == playlist.id
                            && old.songs.iter().any(|saved| saved.video_id == song.video_id))
                })
            };
            if newly_saved { self.queue_download(&song); }
        }
        self.refresh_sidebar();
        self.render_preserving_scroll();
        if let Some(sync) = self.sync.borrow().as_ref() { sync.request_soon(); }
        true
    }

    fn render_preserving_scroll(self: &Rc<Self>) {
        let position = self.scroll.vadjustment().value();
        self.render();
        self.restore_scroll(position);
    }

    fn restore_scroll(self: &Rc<Self>, position: f64) {
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                if ui.generation.get() == generation {
                    if ui.list.last_child().is_some_and(|row| row.height() == 0)
                        || ui.pending_art.borrow().iter().any(|(image, _)| image.height() == 0) {
                        return glib::ControlFlow::Continue;
                    }
                    ui.scroll.vadjustment().set_value(position);
                }
            }
            glib::ControlFlow::Break
        });
    }

    fn saved_songs(&self) -> Vec<Song> {
        let library = self.library.borrow();
        let mut ids = std::collections::HashSet::new();
        library.liked.iter().chain(library.playlists.iter().flat_map(|playlist| &playlist.songs))
            .filter(|song| ids.insert(song.video_id.clone())).cloned().collect()
    }

    fn queue_download(&self, song: &Song) {
        if downloads::local_file(&self.music_directory, &song.video_id).is_none()
            || song.album_art_url.is_some() && downloads::local_art(&self.music_directory, &song.video_id).is_none()
            || downloads::needs_art_upgrade(&self.music_directory, song) {
            if let Some(queue) = self.download_queue.borrow().as_ref() { queue.enqueue(song); }
        }
        self.update_download_spinners();
    }

    fn update_download_spinners(&self) {
        let queue = self.download_queue.borrow();
        let now = std::time::Instant::now();
        self.completed_downloads.borrow_mut().retain(|_, until| *until > now);
        for (id, weak) in self.download_spinners.borrow().iter() {
            if let Some(indicator) = weak.upgrade() {
                let pending = queue.as_ref().is_some_and(|queue| queue.is_pending(id));
                let completed = self.completed_downloads.borrow().contains_key(id);
                indicator.set_visible_child_name(if pending { "downloading" } else { "complete" });
                indicator.set_tooltip_text(Some(if pending { "Download queued or in progress" } else { "Download complete" }));
                indicator.set_visible(pending || completed);
            }
        }
    }

    fn download_completed(self: &Rc<Self>, id: &str) {
        self.completed_downloads.borrow_mut().insert(id.to_owned(), std::time::Instant::now() + Duration::from_secs(2));
        self.update_download_spinners();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            if let Some(ui) = weak.upgrade() { ui.update_download_spinners(); }
        });
    }

    fn start_download_queue(self: &Rc<Self>) {
        let (queue, events) = downloads::Queue::new(self.music_directory.clone(), self.remote.clone());
        *self.download_queue.borrow_mut() = Some(queue);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((song, result)) = events.recv().await {
                let Some(ui) = weak.upgrade() else { break; };
                match result {
                    Ok(_) => {
                        ui.download_completed(&song.video_id);
                        if let Some(path) = downloads::local_art(&ui.music_directory, &song.video_id) {
                            Self::refresh_saved_art(ui.list.upcast_ref(), &song.video_id, &path);
                            if ui.current_song.borrow().as_ref().is_some_and(|current| current.video_id == song.video_id) {
                                ui.playback_art.set_from_file(Some(path));
                            }
                        }
                    }
                    Err(error) => {
                        ui.update_download_spinners();
                        ui.toast(&format!("Could not finish saving {}: {error}. Restart the app to retry.", song.title));
                    }
                }
            }
        });
        // Resume missing or interrupted downloads for the existing library.
        for song in self.saved_songs() { self.queue_download(&song); }
    }

    fn navigate(self: &Rc<Self>, view: View) {
        self.artist_request.set(self.artist_request.get() + 1);
        if let Some(timer) = self.debounce.borrow_mut().take() { timer.remove(); }
        self.search_request.set(self.search_request.get() + 1);
        self.page_loading.set(false);
        self.retry.set_visible(false);
        let previous_view = self.view.replace(view);
        if previous_view != view { self.collection_search.set_text(""); }
        self.artist_back.set_visible(view == View::Artist);
        self.search_controls.set_visible_child_name(if view == View::Search { "search" } else { "title" });
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.status.remove_css_class("error");
        self.refresh_sidebar();
        self.render();
        if self.split.is_collapsed() { self.split.set_show_sidebar(false); }
    }

    fn refresh_sidebar(self: &Rc<Self>) {
        while let Some(child) = self.sidebar.first_child() { self.sidebar.remove(&child); }
        let liked = gtk::Button::new();
        let liked_label = gtk::Label::builder()
            .label("Liked songs")
            .xalign(0.0)
            .build();
        liked.set_child(Some(&liked_label));
        liked.set_widget_name("liked-songs");
        liked.add_css_class(if self.view.get() == View::Liked { "suggested-action" } else { "flat" });
        liked.connect_clicked({ let weak = Rc::downgrade(self); move |_| {
            if let Some(ui) = weak.upgrade() { ui.navigate(View::Liked); }
        }});
        self.sidebar.append(&liked);
        let library = self.library.borrow();
        for playlist in library.sorted_playlists() {
            let button = gtk::Button::new();
            let label = gtk::Label::builder().label(&playlist.name)
                .xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
            button.set_child(Some(&label));
            button.set_tooltip_text(Some(&playlist.name));
            button.set_widget_name(&format!("playlist-{}", playlist.id));
            button.add_css_class(if self.view.get() == View::Playlist(playlist.id) { "suggested-action" } else { "flat" });
            let id = playlist.id;
            button.connect_clicked({ let weak = Rc::downgrade(self); move |_| {
                if let Some(ui) = weak.upgrade() { ui.navigate(View::Playlist(id)); }
            }});
            let context = gtk::GestureClick::new();
            context.set_button(3);
            context.connect_pressed({ let weak = Rc::downgrade(self); let button = button.downgrade(); move |gesture, _, x, y| {
                let (Some(ui), Some(button)) = (weak.upgrade(), button.upgrade()) else { return; };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let menu = gtk::gio::Menu::new();
                menu.append(Some("Rename"), Some("playlist.rename"));
                menu.append(Some("Delete"), Some("playlist.delete"));
                let popover = gtk::PopoverMenu::from_model(Some(&menu));
                popover.set_has_arrow(false);
                popover.set_halign(gtk::Align::Start);
                popover.set_position(gtk::PositionType::Bottom);
                popover.set_parent(&button);
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 0, 0)));
                let actions = gtk::gio::SimpleActionGroup::new();
                let rename = gtk::gio::SimpleAction::new("rename", None);
                let delete = gtk::gio::SimpleAction::new("delete", None);
                for action in [&rename, &delete] { action.set_enabled(ui.writable); actions.add_action(action); }
                popover.insert_action_group("playlist", Some(&actions));
                rename.connect_activate({ let weak = Rc::downgrade(&ui); let popover = popover.downgrade(); move |_, _| {
                    if let Some(popover) = popover.upgrade() { popover.popdown(); }
                    if let Some(ui) = weak.upgrade() { glib::idle_add_local_once(move || ui.rename_playlist(id)); }
                }});
                delete.connect_activate({ let weak = Rc::downgrade(&ui); let popover = popover.downgrade(); move |_, _| {
                    if let Some(popover) = popover.upgrade() { popover.popdown(); }
                    if let Some(ui) = weak.upgrade() {
                        glib::idle_add_local_once(move || {
                            if ui.change(|library| library.delete_playlist(id)) && ui.view.get() == View::Playlist(id) {
                                ui.navigate(View::Liked);
                            }
                        });
                    }
                }});
                popover.connect_closed(|popover| {
                    // GtkModelButton closes its menu before dispatching the action.
                    // Keep the menu and its action group alive until dispatch finishes.
                    let popover = popover.clone();
                    glib::idle_add_local_once(move || popover.unparent());
                });
                popover.popup();
            }});
            button.add_controller(context);
            self.sidebar.append(&button);
        }
    }

    fn rename_playlist(self: &Rc<Self>, id: u64) {
        let Some(name) = self.library.borrow().playlists.iter().find(|p| p.id == id).map(|p| p.name.clone()) else { return; };
        let dialog = adw::AlertDialog::builder().heading("Rename playlist").build();
        let entry = gtk::Entry::builder().text(&name).activates_default(true).build();
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("rename", "Rename");
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        entry.connect_changed({ let dialog = dialog.downgrade(); move |entry| {
            if let Some(dialog) = dialog.upgrade() { dialog.set_response_enabled("rename", !entry.text().trim().is_empty()); }
        }});
        dialog.connect_response(Some("rename"), { let weak = Rc::downgrade(self); let entry = entry.clone(); move |_, _| {
            if let Some(ui) = weak.upgrade() { ui.change(|library| library.rename_playlist(id, entry.text().as_str())); }
        }});
        dialog.present(Some(&self.window));
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    fn playlist_menu(self: &Rc<Self>, song: Option<Song>) -> gtk::MenuButton {
        let menu = gtk::MenuButton::builder().icon_name("list-add-symbolic")
            .tooltip_text("Add to playlist").valign(gtk::Align::Center).build();
        menu.set_sensitive(self.writable);
        let popover = gtk::Popover::new();
        menu.set_popover(Some(&popover));
        popover.connect_show({ let weak = Rc::downgrade(self); move |popover| {
            let Some(ui) = weak.upgrade() else { return; };
            let content = padded_box(8, 8);
            if let Some(song) = &song {
                let label = gtk::Label::new(Some("Add to playlist"));
                label.add_css_class("heading");
                content.append(&label);
                let choices = gtk::Box::new(gtk::Orientation::Vertical, 4);
                for playlist in ui.library.borrow().sorted_playlists() {
                    let contains = playlist.songs.iter().any(|saved| saved.video_id == song.video_id);
                    let button = gtk::Button::with_label(&format!("{}{}", playlist.name, if contains { " ✓" } else { "" }));
                    button.set_sensitive(!contains);
                    let id = playlist.id;
                    let song = song.clone();
                    button.connect_clicked({ let weak = Rc::downgrade(&ui); let popover = popover.downgrade(); move |_| {
                        if let Some(popover) = popover.upgrade() { popover.popdown(); }
                        if let Some(ui) = weak.upgrade() {
                            if ui.change(|library| library.add_song(id, &song).map(|_| ())) { ui.toast("Added to playlist"); }
                        }
                    }});
                    choices.append(&button);
                }
                let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never)
                    .max_content_height(240).propagate_natural_height(true).child(&choices).build();
                content.append(&scroll);
                content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
            let name = gtk::Entry::builder().placeholder_text("New playlist name").width_chars(24).build();
            let create = gtk::Button::with_label(if song.is_some() { "Create playlist and add song" } else { "Create playlist" });
            create.add_css_class("suggested-action");
            let submit = Rc::new({
                let weak = Rc::downgrade(&ui);
                let name = name.clone();
                let song = song.clone();
                let popover = popover.downgrade();
                move || {
                    let Some(ui) = weak.upgrade() else { return; };
                    if name.text().trim().is_empty() { name.grab_focus(); return; }
                    // Close before refreshing rows, which replaces the menu's parent.
                    if let Some(popover) = popover.upgrade() { popover.popdown(); }
                    if ui.change(|library| {
                        let id = library.create_playlist(name.text().as_str())?;
                        if let Some(song) = &song { library.add_song(id, song)?; }
                        Ok(())
                    }) { ui.toast("Playlist created"); }
                }
            });
            create.connect_clicked({ let submit = submit.clone(); move |_| submit() });
            name.connect_activate(move |_| submit());
            content.append(&name);
            content.append(&create);
            popover.set_child(Some(&content));
        }});
        menu
    }

    fn render(self: &Rc<Self>) {
        self.generation.set(self.generation.get() + 1);
        self.pending_art.borrow_mut().clear();
        self.row_play_buttons.borrow_mut().clear();
        self.download_spinners.borrow_mut().clear();
        while let Some(child) = self.list.first_child() { self.list.remove(&child); }
        let songs = match self.view.get() {
            View::Search => {
                self.title.set_text("Search results");
                self.results.borrow().clone()
            }
            View::Liked => {
                self.title.set_text("Liked songs");
                self.library.borrow().liked.clone()
            }
            View::Playlist(id) => {
                let library = self.library.borrow();
                if let Some(playlist) = library.playlists.iter().find(|playlist| playlist.id == id) {
                    self.title.set_text(&playlist.name);
                    playlist.songs.clone()
                } else { self.title.set_text("Playlist deleted"); vec![] }
            }
            View::Artist => {
                if let Some(profile) = self.artist_profile.borrow().as_ref() {
                    self.title.set_text(&profile.artist.link.name);
                    profile.songs.clone()
                } else { self.title.set_text("Artist"); vec![] }
            }
        };
        let collection = matches!(self.view.get(), View::Liked | View::Playlist(_));
        self.collection_search.set_visible(collection);
        self.collection_search.set_placeholder_text(Some(if self.view.get() == View::Liked { "Search liked songs" } else { "Search this playlist" }));
        self.status.remove_css_class("error");
        self.status.set_text(&if songs.is_empty() {
            match self.view.get() {
                View::Search => "Search for songs to start your library.".into(),
                View::Liked => "Tap a song’s heart to save it here.".into(),
                View::Playlist(_) => "This playlist is empty. Add songs with the + button on a song.".into(),
                View::Artist => if self.artist_loading.get() { "Loading artist…".into() }
                    else { self.artist_error.borrow().clone().unwrap_or_else(|| "No top songs available.".into()) },
            }
        } else { format!("{} songs", songs.len()) });
        if self.view.get() == View::Search && self.page_loading.get() {
            self.status.set_text("Searching YouTube Music…");
        } else if self.view.get() == View::Search && !self.artist_results.borrow().is_empty() {
            self.status.set_text(&format!("{} artists · {} songs", self.artist_results.borrow().len(), songs.len()));
        }
        self.list.set_visible(!songs.is_empty() || self.view.get() == View::Artist || !self.artist_results.borrow().is_empty() && self.view.get() == View::Search);
        if self.view.get() == View::Search {
            for artist in self.artist_results.borrow().iter() { self.add_artist_result(artist); }
        } else if self.view.get() == View::Artist {
            if let Some(profile) = self.artist_profile.borrow().as_ref() { self.add_artist_header(profile); }
        }
        let query = self.collection_search.text().trim().to_lowercase();
        let songs: Vec<Song> = songs.into_iter().filter(|song| {
            !collection || query.is_empty() || format!("{} {}", song.title, song.artist.as_deref().unwrap_or("")).to_lowercase().contains(&query)
        }).collect();
        if collection && !query.is_empty() && songs.is_empty() {
            self.list.set_visible(true);
            let empty = gtk::Label::builder().label("No matching songs").margin_top(24).margin_bottom(24).build();
            empty.add_css_class("dim-label");
            self.list.append(&empty);
        }
        self.append_songs(&songs);
    }

    fn return_from_artist(self: &Rc<Self>) {
        let view = self.artist_previous.get();
        let position = self.artist_previous_scroll.get();
        let filter = self.artist_previous_filter.borrow().clone();
        self.navigate(view);
        self.collection_search.set_text(&filter);
        self.restore_scroll(position);
    }

    fn open_artist(self: &Rc<Self>, artist: oxidance::ArtistLink) {
        if self.view.get() != View::Artist {
            self.artist_previous.set(self.view.get());
            self.artist_previous_scroll.set(self.scroll.vadjustment().value());
            *self.artist_previous_filter.borrow_mut() = self.collection_search.text().to_string();
        }
        self.navigate(View::Artist);
        self.artist_loading.set(true);
        self.biography_expanded.set(false);
        self.artist_error.borrow_mut().take();
        self.artist_profile.borrow_mut().take();
        self.title.set_text(&artist.name);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.render();
        self.title.set_text(&artist.name);
        self.scroll.vadjustment().set_value(0.0);
        let request = self.artist_request.get();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = (|| {
                let id = if artist.id.is_empty() {
                    oxidance::artists::search(&artist.name).map_err(|error| error.to_string())?.into_iter()
                        .find(|result| result.link.name.to_lowercase() == artist.name.to_lowercase())
                        .ok_or_else(|| "No matching artist profile found.".to_owned())?.link.id
                } else { artist.id };
                oxidance::artists::profile(&id).map_err(|error| error.to_string())
            })();
            let _ = sender.send_blocking(result);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return; };
            let Some(ui) = weak.upgrade() else { return; };
            if ui.view.get() != View::Artist || ui.artist_request.get() != request { return; }
            ui.artist_loading.set(false);
            ui.spinner.stop();
            ui.spinner.set_visible(false);
            match result {
                Ok(profile) => { *ui.artist_profile.borrow_mut() = Some(profile); }
                Err(error) => { *ui.artist_error.borrow_mut() = Some(format!("Could not load artist: {error}")); }
            }
            ui.render();
        });
    }

    fn add_artist_result(self: &Rc<Self>, artist: &oxidance::artists::Artist) {
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        row.set_title(&artist.link.name);
        row.set_subtitle("Artist");
        row.set_activatable(true);
        row.set_widget_name(&format!("artist-{}", artist.link.id));
        let art = gtk::Image::from_icon_name("avatar-default-symbolic");
        art.set_pixel_size(72);
        art.set_size_request(72, 72);
        art.set_margin_top(8);
        art.set_margin_bottom(8);
        row.add_prefix(&art);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        row.connect_activated({ let weak = Rc::downgrade(self); let artist = artist.link.clone(); move |_| {
            if let Some(ui) = weak.upgrade() { ui.open_artist(artist.clone()); }
        }});
        self.list.append(&row);
        self.queue_art(&art, artist.image_url.as_deref());
    }

    fn queue_art(&self, image: &gtk::Image, url: Option<&str>) {
        if let Some(url) = url {
            let url = if self.view.get() == View::Search { oxidance::search_art_url(url) } else { url.to_owned() };
            if let Some(texture) = self.art_cache.borrow().get(&url) { Self::set_art_texture(image, texture); }
            else { self.pending_art.borrow_mut().push((image.clone(), url)); }
        }
    }

    fn refresh_saved_art(widget: &gtk::Widget, id: &str, path: &std::path::Path) {
        if widget.widget_name() == format!("song-art-{id}") {
            if let Some(image) = widget.downcast_ref::<gtk::Image>() { image.set_from_file(Some(path)); }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            Self::refresh_saved_art(&current, id, path);
            child = current.next_sibling();
        }
    }

    fn set_art_texture(image: &gtk::Image, texture: &gdk::Texture) {
        if image.widget_name() == "artist-profile-image" && texture.width() != texture.height() {
            if let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_read(std::io::Cursor::new(texture.save_to_png_bytes().as_ref().to_vec())) {
                let side = pixbuf.width().min(pixbuf.height());
                let cropped = pixbuf.new_subpixbuf((pixbuf.width() - side) / 2, (pixbuf.height() - side) / 2, side, side);
                image.set_paintable(Some(&gdk::Texture::for_pixbuf(&cropped)));
                return;
            }
        }
        image.set_paintable(Some(texture));
    }

    fn add_artist_header(self: &Rc<Self>, profile: &oxidance::artists::Profile) {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let content = padded_box(16, 16);
        let about = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        let art = gtk::Image::from_icon_name("avatar-default-symbolic");
        art.set_widget_name("artist-profile-image");
        art.set_pixel_size(160);
        art.set_size_request(160, 160);
        art.set_valign(gtk::Align::Start);
        about.append(&art);
        let biography = gtk::Label::builder().label(profile.biography.as_deref().unwrap_or("No biography available."))
            .xalign(0.0).yalign(0.0).wrap(true).selectable(true).hexpand(true).build();
        biography.set_lines(if self.biography_expanded.get() { -1 } else { 2 });
        biography.set_ellipsize(if self.biography_expanded.get() { gtk::pango::EllipsizeMode::None } else { gtk::pango::EllipsizeMode::End });
        let description = gtk::Box::new(gtk::Orientation::Vertical, 8);
        description.set_hexpand(true);
        description.set_valign(gtk::Align::Start);
        description.append(&biography);
        let more = gtk::Button::with_label(if self.biography_expanded.get() { "Show less" } else { "Show more" });
        more.add_css_class("flat");
        more.set_halign(gtk::Align::Start);
        more.set_visible(profile.biography.is_some());
        more.connect_clicked({ let biography = biography.clone(); let weak = Rc::downgrade(self); move |button| {
            let Some(ui) = weak.upgrade() else { return; };
            let expand = !ui.biography_expanded.get();
            ui.biography_expanded.set(expand);
            biography.set_lines(if expand { -1 } else { 2 });
            biography.set_ellipsize(if expand { gtk::pango::EllipsizeMode::None } else { gtk::pango::EllipsizeMode::End });
            button.set_label(if expand { "Show less" } else { "Show more" });
        }});
        description.append(&more);
        about.append(&description);
        content.append(&about);
        let heading = gtk::Label::builder().label("Top songs").xalign(0.0).build();
        heading.add_css_class("heading");
        content.append(&heading);
        row.set_child(Some(&content));
        self.list.append(&row);
        self.queue_art(&art, profile.artist.image_url.as_deref());
    }

    fn link_artist_subtitle(self: &Rc<Self>, widget: &gtk::Widget, markup: &str) {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            if label.has_css_class("subtitle") {
                label.set_can_target(true);
                label.set_markup(markup);
                label.connect_activate_link({ let weak = Rc::downgrade(self); move |_, uri| {
                    if let Some(ui) = weak.upgrade() {
                        if let Some(id) = uri.strip_prefix("artist:") {
                            ui.open_artist(oxidance::ArtistLink { id: id.to_owned(), name: "Artist".into() });
                        } else if let Some(name) = uri.strip_prefix("artist-name:") {
                            ui.open_artist(oxidance::ArtistLink { id: String::new(), name: name.to_owned() });
                        }
                    }
                    glib::Propagation::Stop
                }});
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            self.link_artist_subtitle(&current, markup);
            child = current.next_sibling();
        }
    }

    fn append_songs(self: &Rc<Self>, songs: &[Song]) {
        for song in songs {
            let image = self.add_song(song);
            if let Some(path) = downloads::local_art(&self.music_directory, &song.video_id) {
                image.set_from_file(Some(path));
            } else if let Some(url) = &song.album_art_url {
                let url = if self.view.get() == View::Search { oxidance::search_art_url(url) } else { url.clone() };
                if let Some(texture) = self.art_cache.borrow().get(&url) { image.set_paintable(Some(texture)); }
                else { self.pending_art.borrow_mut().push((image, url)); }
            }
        }
        let weak = Rc::downgrade(self);
        self.scroll.add_tick_callback(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                if ui.pending_art.borrow().iter().any(|(image, _)| image.height() == 0) {
                    return glib::ControlFlow::Continue;
                }
                ui.load_visible_art();
            }
            glib::ControlFlow::Break
        });
    }

    fn load_visible_art(self: &Rc<Self>) {
        let generation = self.generation.get();
        let mut targets: HashMap<String, Vec<gtk::Image>> = HashMap::new();
        self.pending_art.borrow_mut().retain(|(image, url)| {
            let visible = image.height() > 0 && image.compute_bounds(&self.scroll).is_some_and(|bounds| {
                bounds.y() + bounds.height() > -150.0 && bounds.y() < self.scroll.height() as f32 + 150.0
            });
            if visible { targets.entry(url.clone()).or_default().push(image.clone()); }
            !visible
        });
        if targets.is_empty() { return; }
        let urls: Vec<_> = targets.keys().cloned().collect();
        let search_results = self.view.get() == View::Search;
        let (sender, receiver) = async_channel::bounded(8);
        std::thread::spawn(move || {
            let Ok(client) = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build() else { return; };
            for url in urls {
                let fetch = |target: &str| client.get(target).send().and_then(reqwest::blocking::Response::error_for_status)
                    .and_then(reqwest::blocking::Response::bytes);
                let bytes = if search_results { fetch(&url) }
                    else { fetch(&oxidance::high_quality_art_url(&url)).or_else(|_| fetch(&url)) };
                if let Ok(bytes) = bytes {
                    if sender.send_blocking((url, bytes.to_vec())).is_err() { break; }
                }
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((url, bytes)) = receiver.recv().await {
                let Some(ui) = weak.upgrade() else { break; };
                if let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                    ui.art_cache.borrow_mut().insert(url.clone(), texture.clone());
                    if ui.generation.get() == generation {
                        for image in &targets[&url] { Self::set_art_texture(image, &texture); }
                    }
                }
            }
        });
    }

    fn add_song(self: &Rc<Self>, song: &Song) -> gtk::Image {
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        row.set_title(&song.title);
        row.set_subtitle(song.artist.as_deref().unwrap_or("Unknown artist"));
        if !song.artists.is_empty() {
            let markup = song.artists.iter().map(|artist| format!("<a href=\"artist:{}\">{}</a>",
                glib::markup_escape_text(&artist.id), glib::markup_escape_text(&artist.name))).collect::<Vec<_>>().join(", ");
            self.link_artist_subtitle(row.upcast_ref(), &markup);
        } else if let Some(name) = &song.artist {
            let escaped = glib::markup_escape_text(name);
            self.link_artist_subtitle(row.upcast_ref(), &format!("<a href=\"artist-name:{escaped}\">{escaped}</a>"));
        }
        row.set_title_lines(2);
        row.set_subtitle_lines(2);
        row.set_activatable(true);
        row.connect_activated({ let weak = Rc::downgrade(self); let song = song.clone(); move |_| {
            if let Some(ui) = weak.upgrade() { ui.play_song(&song); }
        }});
        let art = gtk::Image::from_icon_name("audio-x-generic-symbolic");
        art.set_widget_name(&format!("song-art-{}", song.video_id));
        art.set_pixel_size(72);
        art.set_size_request(72, 72);
        art.set_margin_top(8);
        art.set_margin_bottom(8);
        row.add_prefix(&art);
        let download_spinner = adw::Spinner::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Download queued or in progress")
            .width_request(16).height_request(16).build();
        let download_indicator = gtk::Stack::builder().valign(gtk::Align::Center).width_request(16).height_request(16).build();
        download_indicator.add_named(&download_spinner, Some("downloading"));
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.set_pixel_size(16);
        download_indicator.add_named(&check, Some("complete"));
        self.download_spinners.borrow_mut().push((song.video_id.clone(), download_indicator.downgrade()));
        row.add_suffix(&download_indicator);
        self.update_download_spinners();
        let play = gtk::Button::builder().icon_name("media-playback-start-symbolic")
            .tooltip_text("Play song").valign(gtk::Align::Center).build();
        play.connect_clicked({ let weak = Rc::downgrade(self); let song = song.clone(); move |_| {
            if let Some(ui) = weak.upgrade() { ui.play_song(&song); }
        }});
        self.row_play_buttons.borrow_mut().push((song.video_id.clone(), play.downgrade()));
        row.add_suffix(&play);
        self.update_playback_buttons();
        let heart = gtk::ToggleButton::builder().valign(gtk::Align::Center).build();
        let liked = self.library.borrow().is_liked(song);
        let symbol = gtk::Label::new(Some(if liked { "♥" } else { "♡" }));
        let icon = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon.set_size_request(16, 16);
        icon.set_halign(gtk::Align::Center);
        symbol.set_halign(gtk::Align::Center);
        icon.append(&symbol);
        heart.set_child(Some(&icon));
        heart.remove_css_class("text-button");
        heart.add_css_class("image-button");
        heart.set_tooltip_text(Some(if liked { "Unlike song" } else { "Like song" }));
        heart.set_active(liked);
        heart.set_sensitive(self.writable);
        if liked { heart.add_css_class("accent"); }
        heart.connect_toggled({ let weak = Rc::downgrade(self); let song = song.clone(); move |_| {
            if let Some(ui) = weak.upgrade() {
                if !ui.change(|library| { library.toggle_like(&song); Ok(()) }) { ui.render(); }
            }
        }});
        row.add_suffix(&heart);
        row.add_suffix(&self.playlist_menu(Some(song.clone())));
        if let View::Playlist(id) = self.view.get() {
            let remove = gtk::Button::builder().icon_name("list-remove-symbolic")
                .tooltip_text("Remove from this playlist").valign(gtk::Align::Center).build();
            remove.set_sensitive(self.writable);
            remove.connect_clicked({ let weak = Rc::downgrade(self); let song = song.clone(); move |_| {
                if let Some(ui) = weak.upgrade() { ui.change(|library| library.remove_song(id, &song)); }
            }});
            row.add_suffix(&remove);
        }
        self.list.append(&row);
        art
    }

    fn update_playback_buttons(&self) {
        self.previous_button.set_sensitive(self.queue_position.get() > 0);
        self.next_button.set_sensitive(self.queue_position.get() + 1 < self.playback_order.borrow().len());
        let playing = self.desired_playing.get();
        let current = self.current_song.borrow();
        self.playback_button.set_icon_name(if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
        self.playback_button.set_tooltip_text(Some(if playing { "Pause" } else { "Play" }));
        for (id, weak) in self.row_play_buttons.borrow().iter() {
            if let Some(button) = weak.upgrade() {
                let active = playing && current.as_ref().is_some_and(|song| &song.video_id == id);
                button.set_icon_name(if active { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
                button.set_tooltip_text(Some(if active { "Pause song" } else { "Play song" }));
            }
        }
    }

    fn stop_player(&self) {
        self.applied_rate.set(1.0);
        self.seek.set_sensitive(false);
        self.seek.set_value(0.0);
        self.elapsed.set_text("0:00");
        self.duration.set_text("—:—");
        self.seek_pending.set(false);
        self.seek_dragging.set(false);
        if let Some(cancel) = self.resolve_cancel.borrow_mut().take() {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.bus_watch.borrow_mut().take();
        if let Some(player) = self.player.borrow_mut().take() { let _ = player.set_state(gst::State::Null); }
        self.resolving.set(false);
        self.buffering.set(false);
    }

    fn playback_failed(&self, error: &str) {
        self.stop_player();
        self.desired_playing.set(false);
        self.toast(&format!("Playback failed: {error}. Press Play to retry."));
        self.update_playback_buttons();
    }

    fn play_song(self: &Rc<Self>, song: &Song) {
        if self.current_song.borrow().as_ref().is_some_and(|current| current.video_id == song.video_id)
            && (self.resolving.get() || self.player.borrow().is_some()) {
            self.toggle_playback();
            return;
        }
        let mut queue = match self.view.get() {
            View::Liked => self.library.borrow().liked.clone(),
            View::Playlist(id) => self.library.borrow().playlists.iter().find(|p| p.id == id).map(|p| p.songs.clone()).unwrap_or_default(),
            View::Search => self.results.borrow().clone(),
            View::Artist => self.artist_profile.borrow().as_ref().map(|p| p.songs.clone()).unwrap_or_default(),
        };
        if !queue.iter().any(|s| s.video_id == song.video_id) { queue.push(song.clone()); }
        *self.playback_order.borrow_mut() = (0..queue.len()).collect();
        self.queue_position.set(queue.iter().position(|s| s.video_id == song.video_id).unwrap());
        *self.playback_queue.borrow_mut() = queue;
        self.reorder_queue();
        self.play_queued_song(song);
    }

    fn reorder_queue(&self) {
        let current = self.playback_order.borrow().get(self.queue_position.get()).copied();
        let mut order: Vec<_> = (0..self.playback_queue.borrow().len()).collect();
        if self.shuffle_button.is_active() {
            if let Some(current) = current { order.retain(|index| *index != current); }
            for i in (1..order.len()).rev() { let j = glib::random_int_range(0, (i + 1) as i32) as usize; order.swap(i, j); }
            if let Some(current) = current { order.insert(0, current); }
        }
        self.queue_position.set(current.and_then(|current| order.iter().position(|index| *index == current)).unwrap_or(0));
        *self.playback_order.borrow_mut() = order;
        self.update_playback_buttons();
    }

    fn advance_queue(self: &Rc<Self>, forward: bool) {
        let position = self.queue_position.get();
        let next = if forward { position.checked_add(1) } else { position.checked_sub(1) };
        let index = next.and_then(|position| self.playback_order.borrow().get(position).copied());
        if let (Some(position), Some(index)) = (next, index) {
            let song = self.playback_queue.borrow()[index].clone();
            let playing = self.desired_playing.get();
            self.queue_position.set(position);
            self.play_queued_song(&song);
            if !playing { self.toggle_playback(); }
        }
    }

    fn play_queued_song(self: &Rc<Self>, song: &Song) {
        self.load_queued_song(song, true);
    }

    fn restore_last_song(self: &Rc<Self>) {
        let Ok(song) = serde_json::from_str::<Song>(&self.settings.string("last-song")) else { return; };
        if song.video_id.is_empty() { return; }
        let mut queue = self.saved_songs();
        if !queue.iter().any(|saved| saved.video_id == song.video_id) { queue.push(song.clone()); }
        self.queue_position.set(queue.iter().position(|saved| saved.video_id == song.video_id).unwrap());
        *self.playback_order.borrow_mut() = (0..queue.len()).collect();
        *self.playback_queue.borrow_mut() = queue;
        self.reorder_queue();
        self.load_queued_song(&song, false);
    }

    fn load_queued_song(self: &Rc<Self>, song: &Song, autoplay: bool) {
        self.stop_player();
        self.playback_generation.set(self.playback_generation.get() + 1);
        let generation = self.playback_generation.get();
        *self.current_song.borrow_mut() = Some(song.clone());
        self.update_artwork_placement();
        self.desired_playing.set(autoplay);
        if let Ok(saved) = serde_json::to_string(song) {
            if let Err(error) = self.settings.set_string("last-song", &saved) { eprintln!("Could not save last song: {error}"); }
        }
        self.resolving.set(true);
        self.playback_bar.set_visible(true);
        self.playback_title.set_text(&format!("{} · {}", song.title, song.artist.as_deref().unwrap_or("Unknown artist")));
        self.update_playback_art(song, generation);
        self.update_playback_buttons();
        if let Some(path) = downloads::local_file(&self.music_directory, &song.video_id) {
            self.resolving.set(false);
            let stream = playback::Stream {
                url: gtk::gio::File::for_path(path).uri().to_string(),
                http_headers: Default::default(),
            };
            if let Err(error) = self.start_stream(stream, generation) { self.playback_failed(&error); }
            return;
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        *self.resolve_cancel.borrow_mut() = Some(cancel.clone());
        let id = song.video_id.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || { let _ = sender.send_blocking(playback::resolve(&id, cancel)); });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return; };
            let Some(ui) = weak.upgrade() else { return; };
            if ui.playback_generation.get() != generation { return; }
            ui.resolving.set(false);
            ui.resolve_cancel.borrow_mut().take();
            match result {
                Ok(stream) => if let Err(error) = ui.start_stream(stream, generation) { ui.playback_failed(&error); },
                Err(error) => ui.playback_failed(&error),
            }
        });
    }

    fn update_playback_art(self: &Rc<Self>, song: &Song, generation: u64) {
        self.playback_art.set_icon_name(Some("audio-x-generic-symbolic"));
        if let Some(path) = downloads::local_art(&self.music_directory, &song.video_id) {
            self.playback_art.set_from_file(Some(path));
            return;
        }
        let Some(url) = &song.album_art_url else { return; };
        let full_url = oxidance::high_quality_art_url(url);
        if let Some(texture) = self.art_cache.borrow().get(&full_url) {
            self.playback_art.set_paintable(Some(texture));
            return;
        }
        let url = url.clone();
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build()
                .and_then(|client| {
                    let fetch = |target: &str| client.get(target).send().and_then(reqwest::blocking::Response::error_for_status)
                        .and_then(reqwest::blocking::Response::bytes);
                    fetch(&oxidance::high_quality_art_url(&url)).or_else(|_| fetch(&url))
                });
            let _ = sender.send_blocking((full_url, result.map(|bytes| bytes.to_vec())));
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((url, Ok(bytes))) = receiver.recv().await else { return; };
            let Some(ui) = weak.upgrade() else { return; };
            if let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                ui.art_cache.borrow_mut().insert(url, texture.clone());
                if ui.playback_generation.get() == generation { ui.playback_art.set_paintable(Some(&texture)); }
            }
        });
    }

    fn toggle_playback(self: &Rc<Self>) {
        if self.player.borrow().is_none() && !self.resolving.get() {
            let song = self.current_song.borrow().clone();
            if let Some(song) = song { self.play_queued_song(&song); }
            return;
        }
        self.desired_playing.set(!self.desired_playing.get());
        let playing = self.desired_playing.get();
        let state = if playing && !self.buffering.get() { gst::State::Playing } else { gst::State::Paused };
        let result = self.player.borrow().as_ref().map(|player| player.set_state(state));
        if result.is_some_and(|result| result.is_err()) { self.playback_failed("Could not change playback state"); return; }
        self.update_playback_buttons();
    }

    fn start_stream(self: &Rc<Self>, stream: playback::Stream, generation: u64) -> Result<(), String> {
        gst::init().map_err(|error| error.to_string())?;
        let player = gst::ElementFactory::make("playbin").build().map_err(|error| error.to_string())?;
        // Audio only: no video sink, visualization, or disk download buffering.
        player.set_property_from_str("flags", "audio+buffering");
        player.set_property("audio-filter", playback::speed_filter()?);
        player.set_property("volume", self.volume.get());
        player.set_property("uri", &stream.url);
        #[cfg(test)]
        {
            // Decode and advance the real stream without producing sound in UI tests.
            let sink = gst::ElementFactory::make("fakesink").property("sync", true)
                .build().map_err(|error| error.to_string())?;
            player.set_property("audio-sink", sink);
        }
        player.connect("source-setup", false, move |values| {
            if let Ok(source) = values[1].get::<gst::Element>() {
                if source.find_property("user-agent").is_some() {
                    if let Some(agent) = stream.http_headers.get("User-Agent") { source.set_property("user-agent", agent); }
                }
                if source.find_property("extra-headers").is_some() {
                    let mut headers = gst::Structure::builder("headers");
                    for (name, value) in &stream.http_headers { headers = headers.field(name, value); }
                    source.set_property("extra-headers", headers.build());
                }
            }
            None
        });
        let weak = Rc::downgrade(self);
        let guard = player.bus().ok_or("Playback bus unavailable")?.add_watch_local(move |_, message| {
            let Some(ui) = weak.upgrade() else { return glib::ControlFlow::Break; };
            if ui.playback_generation.get() != generation { return glib::ControlFlow::Break; }
            match message.view() {
                gst::MessageView::AsyncDone(_) => {
                    let sought = ui.seek_pending.replace(false);
                    ui.update_timeline();
                    if sought { if let Some(service) = ui.mpris.borrow().as_ref() { service.seeked(&ui); } }
                }
                gst::MessageView::Error(error) => ui.playback_failed(&error.error().to_string()),
                gst::MessageView::Eos(_) => {
                    if ui.queue_position.get() + 1 < ui.playback_order.borrow().len() { ui.advance_queue(true); return glib::ControlFlow::Continue; }
                    ui.stop_player();
                    ui.desired_playing.set(false);
                    ui.update_playback_buttons();
                }
                gst::MessageView::Buffering(buffering) => {
                    let loading = buffering.percent() < 100;
                    ui.buffering.set(loading);
                    if let Some(player) = ui.player.borrow().as_ref() {
                        let _ = player.set_state(if ui.desired_playing.get() && !loading { gst::State::Playing } else { gst::State::Paused });
                    }
                }
                _ => (),
            }
            glib::ControlFlow::Continue
        }).map_err(|error| error.to_string())?;
        *self.bus_watch.borrow_mut() = Some(guard);
        *self.player.borrow_mut() = Some(player.clone());
        player.set_state(if self.desired_playing.get() { gst::State::Playing } else { gst::State::Paused })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn update_timeline(&self) {
        let player = self.player.borrow();
        let Some(player) = player.as_ref() else { return; };
        let Some(mut duration) = player.query_duration::<gst::ClockTime>() else { return; };
        if duration.is_zero() { return; }
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        let seekable = player.query(&mut query) && query.result().0;
        if seekable && self.applied_rate.get() != self.playback_rate.get() {
            if let Some(position) = player.query_position::<gst::ClockTime>() {
                if player.seek(self.playback_rate.get(), gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                    gst::SeekType::Set, position, gst::SeekType::None, gst::ClockTime::NONE).is_ok() {
                    self.applied_rate.set(self.playback_rate.get());
                    self.seek_pending.set(true);
                    return; // Wait for the new segment before querying its scaled timeline.
                } else {
                    self.playback_rate.set(self.applied_rate.get());
                    self.toast("This song could not change playback speed.");
                }
            }
        }
        self.seek.set_sensitive(seekable);
        self.duration.set_text(&format_time(duration.seconds()));
        duration = gst::ClockTime::from_nseconds((duration.nseconds() as f64 * self.applied_rate.get()) as u64);
        self.seek.set_range(0.0, duration.nseconds() as f64 / 1_000_000_000.0);
        if !self.seek_pending.get() && !self.seek_dragging.get() {
            if let Some(position) = player.query_position::<gst::ClockTime>() {
                self.seek.set_value(position.nseconds() as f64 / 1_000_000_000.0 * self.applied_rate.get());
                self.elapsed.set_text(&format_time(position.seconds()));
            }
        }
    }

    fn seek_to(&self, seconds: f64) {
        if !seconds.is_finite() || !self.seek.is_sensitive() { return; }
        let seconds = seconds.clamp(0.0, self.seek.adjustment().upper());
        let position = gst::ClockTime::from_nseconds((seconds / self.applied_rate.get() * 1_000_000_000.0) as u64);
        let success = self.player.borrow().as_ref().is_some_and(|player| {
            player.seek(self.applied_rate.get(), gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set, position, gst::SeekType::None, gst::ClockTime::NONE).is_ok()
        });
        if success {
            self.seek_pending.set(true);
            self.seek.set_value(seconds);
            self.elapsed.set_text(&format_time((seconds / self.applied_rate.get()) as u64));
        } else {
            self.toast("This stream could not seek to that position.");
        }
    }

    fn schedule_search(self: &Rc<Self>) {
        if let Some(timer) = self.debounce.borrow_mut().take() { timer.remove(); }
        self.search_request.set(self.search_request.get() + 1);
        self.page_loading.set(false);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.retry.set_visible(false);
        if self.entry.text().trim().is_empty() {
            self.cursor.borrow_mut().take();
            self.results.borrow_mut().clear();
            self.artist_results.borrow_mut().clear();
            if self.view.get() == View::Search { self.render(); }
            return;
        }
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(Duration::from_secs(1), move || {
            if let Some(ui) = weak.upgrade() {
                ui.debounce.borrow_mut().take();
                ui.search();
            }
        });
        *self.debounce.borrow_mut() = Some(timer);
    }

    fn search(self: &Rc<Self>) {
        if let Some(timer) = self.debounce.borrow_mut().take() { timer.remove(); }
        let query = self.entry.text().trim().to_owned();
        if query.is_empty() { return; }
        self.search_request.set(self.search_request.get() + 1);
        *self.query.borrow_mut() = query;
        self.cursor.borrow_mut().take();
        self.view.set(View::Search);
        self.artist_back.set_visible(false);
        self.artist_request.set(self.artist_request.get() + 1);
        self.results.borrow_mut().clear();
        self.artist_results.borrow_mut().clear();
        self.pagination_failed.set(false);
        self.refresh_sidebar();
        self.render();
        self.scroll.vadjustment().set_value(0.0);
        if self.split.is_collapsed() { self.split.set_show_sidebar(false); }
        self.fetch_page(false);
    }

    fn maybe_load_more(self: &Rc<Self>) {
        let adjustment = self.scroll.vadjustment();
        if adjustment.upper() > 0.0 && adjustment.value() + adjustment.page_size() >= adjustment.upper() - 250.0 {
            self.load_more();
        }
    }

    fn load_more(self: &Rc<Self>) {
        if self.view.get() != View::Search || self.page_loading.get() || self.pagination_failed.get()
            || self.debounce.borrow().is_some() || self.entry.text().trim() != *self.query.borrow()
            || self.cursor.borrow().is_none() { return; }
        self.fetch_page(true);
    }

    fn fetch_page(self: &Rc<Self>, append: bool) {
        self.page_loading.set(true);
        let request = self.search_request.get();
        let query = self.query.borrow().clone();
        let cursor = if append { self.cursor.borrow().clone() } else { None };
        self.retry.set_visible(false);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.status.remove_css_class("error");
        self.status.set_text(if append { "Loading more songs…" } else { "Searching YouTube Music…" });
        let (sender, receiver) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let artists = if append { Ok(vec![]) } else { oxidance::artists::search(&query).map_err(|error| error.to_string()) };
            let result = oxidance::search_page(&query, cursor).map(|page| (page, artists)).map_err(|error| error.to_string());
            let _ = sender.send_blocking(result);
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = receiver.recv().await else { return; };
            let Some(ui) = weak.upgrade() else { return; };
            if ui.search_request.get() != request || ui.view.get() != View::Search { return; }
            ui.page_loading.set(false);
            ui.spinner.stop();
            ui.spinner.set_visible(false);
            match result {
                Ok((page, artists)) => {
                    *ui.cursor.borrow_mut() = page.next;
                    if !append {
                        match artists {
                            Ok(artists) => {
                                *ui.artist_results.borrow_mut() = artists;
                                for artist in ui.artist_results.borrow().iter() { ui.add_artist_result(artist); }
                            }
                            Err(error) => ui.toast(&format!("Artist search unavailable: {error}")),
                        }
                    }
                    ui.results.borrow_mut().extend(page.songs.clone());
                    ui.list.set_visible(!ui.results.borrow().is_empty() || !ui.artist_results.borrow().is_empty());
                    ui.append_songs(&page.songs);
                    let count = ui.results.borrow().len();
                    let artist_count = ui.artist_results.borrow().len();
                    ui.status.set_text(&if artist_count > 0 { format!("{artist_count} artists · {count} songs") }
                        else if count == 0 { "No songs found. Try another query.".into() }
                        else if ui.cursor.borrow().is_none() { format!("{count} songs · End of results") }
                        else { format!("{count} songs") });
                }
                Err(error) => {
                    ui.pagination_failed.set(true);
                    ui.retry.set_visible(true);
                    ui.status.set_text(&format!("Could not fetch songs: {error}"));
                    ui.status.add_css_class("error");
                }
            }
        });
    }

}

impl Drop for Ui {
    fn drop(&mut self) { self.stop_player(); }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("io.github.oxidance.Oxidance").build();
    gtk::gio::resources_register_include!("oxidance.gresource").expect("Could not register the app icon");
    app.connect_startup(|_| {
        if let Some(display) = gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_resource_path("/io/github/oxidance/Oxidance/icons");
        }
        gtk::Window::set_default_icon_name("io.github.oxidance.Oxidance");
    });
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() { window.present(); return; }
        let ui = Ui::new(app, glib::user_data_dir().join("oxidance/library.json"));
        match mpris::Service::new(&ui) {
            Ok(service) => *ui.mpris.borrow_mut() = Some(service),
            Err(error) => eprintln!("MPRIS unavailable: {error}"),
        }
        let window = ui.window.clone();
        let lifetime = RefCell::new(Some(ui));
        window.connect_close_request(move |_| {
            lifetime.borrow_mut().take();
            glib::Propagation::Proceed
        });
    });
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a desktop display"]
    fn download_spinner_animates() {
        adw::init().unwrap();
        let spinner = adw::Spinner::builder().width_request(20).height_request(20)
            .halign(gtk::Align::Center).valign(gtk::Align::Center).build();
        let window = gtk::Window::builder().default_width(120).default_height(120).child(&spinner).build();
        let paintable = gtk::WidgetPaintable::new(Some(&spinner));
        let frames = Rc::new(Cell::new(0));
        paintable.connect_invalidate_contents({ let frames = frames.clone(); move |_| frames.set(frames.get() + 1) });
        window.present();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            pump();
            paintable.snapshot(&gtk::Snapshot::new(), 20.0, 20.0);
            std::thread::sleep(Duration::from_millis(20));
        }
        window.close();
        assert!(frames.get() > 5, "expected multiple animated frames, got {}", frames.get());
    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut widgets = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(current) = child {
            widgets.extend(descendants(&current));
            child = current.next_sibling();
        }
        widgets
    }

    fn pump() {
        let context = glib::MainContext::default();
        while context.pending() { context.iteration(false); }
    }

    fn heart(ui: &Ui) -> gtk::ToggleButton {
        descendants(ui.list.upcast_ref()).into_iter()
            .find_map(|widget| widget.downcast::<gtk::ToggleButton>().ok()).unwrap()
    }

    fn song_menu(ui: &Ui) -> gtk::MenuButton {
        descendants(ui.list.upcast_ref()).into_iter()
            .find_map(|widget| widget.downcast::<gtk::MenuButton>().ok()).unwrap()
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_artist_back_restores_collection_position() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.ArtistBackTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, std::env::temp_dir().join(format!("oxidance-back-test-{}/library.json", std::process::id())));
        ui.library.borrow_mut().liked = (0..40).map(|index| Song { video_id: format!("song{index}"), title: format!("Track {index}"), artist: Some("Artist".into()), album_art_url: None, artists: vec![] }).collect();
        let settle = || {
            let deadline = std::time::Instant::now() + Duration::from_millis(250);
            while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        };
        ui.navigate(View::Liked);
        ui.collection_search.set_text("Track");
        settle();
        ui.scroll.vadjustment().set_value(800.0);
        let position = ui.scroll.vadjustment().value();
        assert!(position > 0.0);
        ui.open_artist(oxidance::ArtistLink { id: "UCmissing".into(), name: "Artist".into() });
        assert_eq!(ui.artist_previous_scroll.get(), position);
        ui.artist_back.emit_clicked();
        settle();
        assert!(ui.view.get() == View::Liked);
        assert_eq!(ui.collection_search.text(), "Track");
        assert!((ui.scroll.vadjustment().value() - position).abs() < 1.0, "Back must restore the collection scroll position");
        ui.window.close();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_mpris_controls_and_artwork() {
        adw::init().unwrap();
        gst::init().unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-mpris-test-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("Music")).unwrap();
        let audio = directory.join("Music/First [first].wav");
        let generator = gst::parse::launch(&format!("audiotestsrc num-buffers=3000 samplesperbuffer=480 ! audio/x-raw,rate=48000 ! wavenc ! filesink location=\"{}\"", audio.display())).unwrap();
        generator.set_state(gst::State::Playing).unwrap();
        let message = generator.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos, gst::MessageType::Error]).unwrap();
        generator.set_state(gst::State::Null).unwrap();
        assert!(matches!(message.view(), gst::MessageView::Eos(_)));
        std::fs::copy(&audio, directory.join("Music/Second [second].wav")).unwrap();
        let cover = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 32, 32).unwrap();
        cover.fill(0xff0000ff);
        std::fs::write(audio.with_extension("cover"), cover.save_to_bufferv("png", &[]).unwrap()).unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.MprisTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, directory.join("library.json"));
        *ui.mpris.borrow_mut() = Some(mpris::Service::new(&ui).unwrap());
        let first = Song { video_id: "first".into(), title: "First".into(), artist: Some("Artist".into()), album_art_url: None, artists: vec![] };
        let second = Song { video_id: "second".into(), title: "Second".into(), ..first.clone() };
        ui.library.borrow_mut().liked = vec![first.clone(), second];
        ui.navigate(View::Liked);
        ui.play_song(&first);
        let call = |method: &str, arguments: &[&str]| {
            let method = method.to_owned();
            let arguments: Vec<String> = arguments.iter().map(|argument| argument.to_string()).collect();
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let output = std::process::Command::new("gdbus").args(["call", "--session", "--dest", &mpris::bus_name(),
                    "--object-path", "/org/mpris/MediaPlayer2", "--method", &method]).args(arguments).output().unwrap();
                sender.send(output).unwrap();
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                pump();
                if let Ok(output) = receiver.try_recv() {
                    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                    break String::from_utf8(output.stdout).unwrap();
                }
                assert!(std::time::Instant::now() < deadline, "D-Bus call timed out");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let properties = call("org.freedesktop.DBus.Properties.GetAll", &["org.mpris.MediaPlayer2.Player"]);
        assert!(properties.contains("First") && properties.contains("Artist"));
        assert!(properties.contains("file://") && properties.contains(".cover"));
        assert!(properties.contains("Playing"));
        call("org.mpris.MediaPlayer2.Player.Pause", &[]);
        assert!(!ui.desired_playing.get());
        ui.navigate(View::Search); // Changing pages must not change the established queue.
        call("org.mpris.MediaPlayer2.Player.Next", &[]);
        assert_eq!(ui.current_song.borrow().as_ref().unwrap().video_id, "second");
        assert!(!ui.desired_playing.get());
        call("org.mpris.MediaPlayer2.Player.Previous", &[]);
        assert_eq!(ui.current_song.borrow().as_ref().unwrap().video_id, "first");
        call("org.freedesktop.DBus.Properties.Set", &["org.mpris.MediaPlayer2.Player", "Shuffle", "<true>"]);
        assert!(ui.shuffle_button.is_active());
        assert_eq!(ui.playback_order.borrow().len(), 2);
        assert_eq!(ui.playback_queue.borrow()[ui.playback_order.borrow()[0]].video_id, "first");
        call("org.freedesktop.DBus.Properties.Set", &["org.mpris.MediaPlayer2.Player", "Rate", "<1.5>"]);
        assert_eq!(ui.playback_rate.get(), 1.5);
        call("org.freedesktop.DBus.Properties.Set", &["org.mpris.MediaPlayer2.Player", "Volume", "<0.4>"]);
        assert_eq!(ui.volume.get(), 0.4);
        call("org.mpris.MediaPlayer2.Player.PlayPause", &[]);
        assert!(ui.desired_playing.get());
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        call("org.mpris.MediaPlayer2.Player.SetPosition", &["/org/mpris/MediaPlayer2/track/6669727374", "29900000"]);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while ui.current_song.borrow().as_ref().unwrap().video_id == "first" && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ui.current_song.borrow().as_ref().unwrap().video_id, "second", "finishing a song must advance the queue");
        assert_eq!(ui.player.borrow().as_ref().unwrap().property::<f64>("volume"), 0.4);
        call("org.mpris.MediaPlayer2.Player.Stop", &[]);
        assert!(ui.player.borrow().is_none());
        let stopped = call("org.freedesktop.DBus.Properties.Get", &["org.mpris.MediaPlayer2.Player", "PlaybackStatus"]);
        assert!(stopped.contains("Stopped"));
        let saved = ui.settings.string("last-song");
        assert!(saved.contains("second"));
        ui.current_song.borrow_mut().take();
        ui.restore_last_song();
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(ui.current_song.borrow().as_ref().unwrap().video_id, "second");
        assert!(!ui.desired_playing.get(), "restored songs must never autoplay");
        assert_eq!(ui.player.borrow().as_ref().unwrap().current_state(), gst::State::Paused);
        ui.stop_player();
        ui.mpris.borrow_mut().take();
        ui.window.close();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_collection_search() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.CollectionSearchTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, std::env::temp_dir().join(format!("oxidance-collection-test-{}/library.json", std::process::id())));
        let first = Song { video_id: "first".into(), title: "Blind Spots".into(), artist: Some("C418".into()), album_art_url: None, artists: vec![] };
        let second = Song { video_id: "second".into(), title: "By and By".into(), artist: Some("nitsua".into()), album_art_url: None, artists: vec![] };
        ui.library.borrow_mut().liked = vec![first.clone(), second];
        let id = ui.library.borrow_mut().create_playlist("Test").unwrap();
        ui.library.borrow_mut().playlists[0].songs.push(first);
        let labels = || descendants(ui.list.upcast_ref()).into_iter().filter_map(|w| w.downcast::<gtk::Label>().ok()).map(|label| label.text().to_string()).collect::<Vec<_>>();
        ui.navigate(View::Liked);
        assert!(gtk::prelude::WidgetExt::is_visible(&ui.collection_search));
        ui.collection_search.set_text("  NITSUA  ");
        assert!(labels().contains(&"By and By".into()));
        assert!(!labels().contains(&"Blind Spots".into()));
        assert_eq!(ui.status.text(), "2 songs");
        ui.navigate(View::Playlist(id));
        assert!(ui.collection_search.text().is_empty());
        ui.collection_search.set_text("nitsua");
        assert!(labels().contains(&"No matching songs".into()));
        assert!(!labels().contains(&"By and By".into()));
        ui.collection_search.set_text("blind");
        assert!(labels().contains(&"Blind Spots".into()));
        assert_eq!(ui.library.borrow().liked.len(), 2);
        ui.navigate(View::Search);
        assert!(!gtk::prelude::WidgetExt::is_visible(&ui.collection_search));
        assert!(!ui.page_loading.get());
        ui.window.close();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_playlist_context_delete() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.ContextTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-context-test-{}", std::process::id()));
        let path = directory.join("library.json");
        let ui = Ui::new(&app, path.clone());
        assert!(ui.change(|library| library.create_playlist("kl;j").map(|_| ())));
        let id = ui.library.borrow().playlists[0].id;
        ui.navigate(View::Playlist(id));
        pump();
        let button = descendants(ui.sidebar.upcast_ref()).into_iter().find(|widget| widget.widget_name() == format!("playlist-{id}")).unwrap();
        let controllers = button.observe_controllers();
        let gesture = (0..controllers.n_items()).find_map(|index| controllers.item(index).unwrap().downcast::<gtk::GestureClick>().ok().filter(|gesture| gesture.button() == 3)).unwrap();
        gesture.emit_by_name::<()>("pressed", &[&1_i32, &20.0_f64, &20.0_f64]);
        pump();
        let popover = descendants(&button).into_iter().find_map(|widget| widget.downcast::<gtk::PopoverMenu>().ok()).unwrap();
        let delete = descendants(popover.upcast_ref()).into_iter().find(|widget| {
            widget.type_().name() == "GtkModelButton" && descendants(widget).iter().any(|child| child.downcast_ref::<gtk::Label>().is_some_and(|label| label.text() == "Delete"))
        }).expect("Delete menu item must exist");
        assert!(delete.activate());
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!(ui.library.borrow().playlists.is_empty(), "activating the actual Delete menu item must delete the playlist");
        assert!(Library::load(&path).unwrap().playlists.is_empty());
        assert!(ui.view.get() == View::Liked);
        ui.window.close();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_artist_like_preserves_scroll() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.ScrollTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-scroll-test-{}", std::process::id()));
        let ui = Ui::new(&app, directory.join("library.json"));
        let song = Song { video_id: "first".into(), title: "First".into(), artist: None, album_art_url: None, artists: vec![] };
        *ui.artist_profile.borrow_mut() = Some(oxidance::artists::Profile {
            artist: oxidance::artists::Artist { link: oxidance::ArtistLink { id: "UCtest".into(), name: "Test artist".into() }, image_url: None },
            biography: Some("Biography\n".repeat(20)),
            songs: (0..30).map(|index| Song { video_id: if index == 0 { song.video_id.clone() } else { format!("song{index}") }, ..song.clone() }).collect(),
        });
        ui.biography_expanded.set(true);
        ui.navigate(View::Artist);
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        ui.scroll.vadjustment().set_value(800.0);
        let position = ui.scroll.vadjustment().value();
        assert!(position > 0.0);
        heart(&ui).set_active(true);
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!((ui.scroll.vadjustment().value() - position).abs() < 1.0, "liking must preserve scroll position");
        assert!(ui.biography_expanded.get());
        assert!(ui.library.borrow().is_liked(&song));
        ui.split.set_show_sidebar(false);
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        ui.scroll.vadjustment().set_value(800.0);
        let toggle = descendants(ui.window.upcast_ref()).into_iter().filter_map(|widget| widget.downcast::<gtk::ToggleButton>().ok())
            .find(|button| button.is_visible() && button.tooltip_text().as_deref() == Some("Show or hide library sidebar")).unwrap();
        toggle.grab_focus();
        toggle.set_active(true);
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!((ui.scroll.vadjustment().value() - 800.0).abs() < 1.0, "opening the sidebar must preserve scroll position");
        ui.window.close();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_offline_saved_art() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.OfflineArtTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-offline-art-test-{}", std::process::id()));
        let ui = Ui::new(&app, directory.join("library.json"));
        std::fs::create_dir_all(&ui.music_directory).unwrap();
        let audio = ui.music_directory.join("Saved [offline].webm");
        std::fs::write(&audio, "audio").unwrap();
        let pixbuf = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 2, 2).unwrap();
        pixbuf.fill(0x336699ff);
        std::fs::write(audio.with_extension("cover"), pixbuf.save_to_bufferv("png", &[]).unwrap()).unwrap();
        let song = Song { video_id: "offline".into(), title: "Offline song".into(), artist: None,
            album_art_url: Some("http://127.0.0.1:9/unavailable".into()), artists: vec![] };
        ui.library.borrow_mut().toggle_like(&song);
        ui.navigate(View::Liked);
        let image = descendants(ui.list.upcast_ref()).into_iter().find_map(|widget| {
            (widget.widget_name() == "song-art-offline").then(|| widget.downcast::<gtk::Image>().ok()).flatten()
        }).unwrap();
        assert!(image.paintable().is_some_and(|paintable| paintable.is::<gdk::Texture>()));
        assert!(ui.pending_art.borrow().is_empty(), "offline artwork must not enqueue network requests");
        assert!(ui.art_cache.borrow().is_empty(), "artwork must work without an in-memory cache");
        ui.update_playback_art(&song, ui.playback_generation.get());
        assert!(ui.playback_art.paintable().is_some_and(|paintable| paintable.is::<gdk::Texture>()));
        ui.download_completed(&song.video_id);
        ui.render();
        let indicator = ui.download_spinners.borrow()[0].1.upgrade().unwrap();
        assert!(indicator.is_visible());
        assert_eq!(indicator.visible_child_name().as_deref(), Some("complete"));
        let deadline = std::time::Instant::now() + Duration::from_millis(2100);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!(!indicator.is_visible(), "completion check mark must disappear after two seconds");
        ui.window.close();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_resampled_speed_and_seek() {
        adw::init().unwrap();
        gst::init().unwrap();
        let path = std::env::temp_dir().join(format!("oxidance-rate-{}.wav", std::process::id()));
        let generator = gst::parse::launch(&format!("audiotestsrc num-buffers=3000 samplesperbuffer=480 ! audio/x-raw,rate=48000 ! wavenc ! filesink location={}", path.display())).unwrap();
        generator.set_state(gst::State::Playing).unwrap();
        let message = generator.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos, gst::MessageType::Error]).unwrap();
        generator.set_state(gst::State::Null).unwrap();
        assert!(matches!(message.view(), gst::MessageView::Eos(_)));
        let app = adw::Application::builder().application_id("io.github.oxidance.RateTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, std::env::temp_dir().join(format!("oxidance-rate-test-{}/library.json", std::process::id())));
        ui.start_stream(playback::Stream { url: gtk::gio::File::for_path(&path).uri().to_string(), http_headers: Default::default() }, ui.playback_generation.get()).unwrap();
        let settle = || {
            let deadline = std::time::Instant::now() + Duration::from_millis(400);
            while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
            ui.update_timeline();
        };
        settle();
        assert_eq!(ui.duration.text(), "0:30");
        *ui.current_song.borrow_mut() = Some(Song { video_id: "tone".into(), title: "Tone".into(), artist: None, album_art_url: None, artists: vec![] });
        ui.playback_bar.set_visible(true);
        let controllers = ui.window.observe_controllers();
        let keys = (0..controllers.n_items()).find_map(|index| controllers.item(index).unwrap().downcast::<gtk::EventControllerKey>().ok()).unwrap();
        ui.entry.grab_focus();
        assert!(!keys.emit_by_name::<bool>("key-pressed", &[&gdk::Key::space, &0_u32, &gdk::ModifierType::empty()]));
        assert!(!ui.desired_playing.get());
        ui.playback_button.grab_focus();
        assert!(keys.emit_by_name::<bool>("key-pressed", &[&gdk::Key::space, &0_u32, &gdk::ModifierType::empty()]));
        assert!(ui.desired_playing.get());
        assert!(keys.emit_by_name::<bool>("key-pressed", &[&gdk::Key::space, &0_u32, &gdk::ModifierType::empty()]));
        assert!(!ui.desired_playing.get());
        ui.seek_to(12.0);
        settle();
        assert_eq!(ui.elapsed.text(), "0:12");
        for (rate, elapsed, duration) in [(1.5, "0:08", "0:20"), (2.0, "0:06", "0:15"), (0.5, "0:24", "1:00"), (1.0, "0:12", "0:30")] {
            ui.playback_rate.set(rate);
            ui.update_timeline();
            settle();
            assert_eq!(ui.elapsed.text(), elapsed);
            assert_eq!(ui.duration.text(), duration);
            assert!((ui.seek.value() - 12.0).abs() < 0.05);
            assert!((ui.seek.adjustment().upper() - 30.0).abs() < 0.05);
            ui.seek_to(18.0);
            settle();
            assert!((ui.seek.value() - 18.0).abs() < 0.05);
            ui.seek_to(12.0);
            settle();
        }
        ui.stop_player();
        ui.window.close();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_artwork_preferences() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.ArtworkTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, std::env::temp_dir().join(format!("oxidance-artwork-test-{}/library.json", std::process::id())));
        *ui.current_song.borrow_mut() = Some(Song { video_id: "example".into(), title: "Example".into(), artist: None, album_art_url: None, artists: vec![] });
        ui.playback_bar.set_visible(true);
        gtk::prelude::WidgetExt::activate_action(&ui.window, "win.preferences", None).unwrap();
        pump();
        let row = descendants(ui.window.upcast_ref()).into_iter()
            .find_map(|widget| widget.downcast::<adw::SwitchRow>().ok()).unwrap();
        row.set_active(true);
        pump();
        assert!(ui.settings.boolean("artwork-in-sidebar"));
        assert!(ui.sidebar_art.is_visible());
        assert!(!ui.playback_art.is_visible());
        ui.window.visible_dialog().unwrap().force_close();
        pump();
        ui.split.set_show_sidebar(false);
        assert!(!ui.split.shows_sidebar());
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!(!ui.sidebar_art.is_visible());
        assert!(ui.playback_art.is_visible());
        ui.split.set_show_sidebar(true);
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!(ui.sidebar_art.is_visible());
        row.set_active(false);
        pump();
        assert!(!ui.sidebar_art.is_visible());
        assert!(ui.playback_art.is_visible());
        ui.window.close();
    }

    #[test]
    #[ignore = "requires a desktop display"]
    fn ui_speed_scroll_and_settings() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.SpeedTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let ui = Ui::new(&app, std::env::temp_dir().join(format!("oxidance-speed-test-{}/library.json", std::process::id())));
        let menu = descendants(ui.window.upcast_ref()).into_iter().filter_map(|widget| widget.downcast::<gtk::MenuButton>().ok())
            .find(|button| button.tooltip_text().as_deref() == Some("Playback speed")).unwrap();
        let slider = descendants(menu.popover().unwrap().upcast_ref()).into_iter()
            .find_map(|widget| widget.downcast::<gtk::Scale>().ok()).unwrap();
        let controllers = slider.observe_controllers();
        let scroll = (0..controllers.n_items()).find_map(|index| controllers.item(index).unwrap().downcast::<gtk::EventControllerScroll>().ok()).unwrap();
        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &-1.0_f64]);
        assert!((slider.value() - 1.01).abs() < 1e-8);
        assert_eq!(ui.settings.double("playback-speed"), 1.01);
        scroll.emit_by_name::<bool>("scroll", &[&0.0_f64, &1.0_f64]);
        assert_eq!(slider.value(), 1.0);
        ui.window.close();
    }

    #[test]
    #[ignore = "requires a desktop display and live YouTube Music access"]
    fn ui_artist_search_and_profile() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.ArtistTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-artist-test-{}", std::process::id()));
        let ui = Ui::new(&app, directory.join("library.json"));
        ui.entry.set_text("nujabes");
        ui.entry.grab_focus();
        ui.entry.set_position(3);
        ui.entry.emit_by_name::<()>("activate", &[]);
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while ui.page_loading.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!ui.artist_results.borrow().is_empty(), "artist search must return results");
        let first = ui.list.first_child().unwrap().downcast::<adw::ActionRow>().unwrap();
        assert_eq!(first.title(), "Nujabes", "exact artist match must be first");
        assert_eq!(first.subtitle().as_deref(), Some("Artist"));
        assert_eq!(ui.search_controls.visible_child_name().as_deref(), Some("search"), "the search field stays open");
        let focus = gtk::prelude::GtkWindowExt::focus(&ui.window).unwrap();
        assert!(focus == *ui.entry.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&ui.entry), "the search field keeps focus");
        assert_eq!(ui.entry.position(), 3, "the cursor stays in place");
        first.emit_by_name::<()>("activated", &[]);
        assert!(ui.view.get() == View::Artist);
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while ui.artist_loading.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ui.artist_error.borrow().is_none(), "{:?}", ui.artist_error.borrow());
        assert_eq!(ui.title.text(), "Nujabes");
        assert!(ui.artist_profile.borrow().as_ref().unwrap().biography.is_some());
        assert!(ui.artist_profile.borrow().as_ref().unwrap().songs.len() > 5);
        let image = descendants(ui.list.upcast_ref()).into_iter().find_map(|widget| {
            (widget.widget_name() == "artist-profile-image").then(|| widget.downcast::<gtk::Image>().ok()).flatten()
        }).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !image.paintable().is_some_and(|paintable| paintable.is::<gdk::Texture>()) && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        let paintable = image.paintable().unwrap();
        assert!(paintable.is::<gdk::Texture>());
        assert_eq!(paintable.intrinsic_width(), paintable.intrinsic_height(), "profile artwork must be cropped square");
        let more = descendants(ui.list.upcast_ref()).into_iter().filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some("Show more")).unwrap();
        let bio = descendants(ui.list.upcast_ref()).into_iter().filter_map(|widget| widget.downcast::<gtk::Label>().ok())
            .find(|label| label.lines() == 2).unwrap();
        more.emit_clicked();
        assert_eq!(bio.lines(), -1);
        assert_eq!(more.label().as_deref(), Some("Show less"));
        more.emit_clicked();
        assert_eq!(bio.lines(), 2);
        let labels: Vec<_> = descendants(ui.list.upcast_ref()).into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Label>().ok()).collect();
        let artist_label = labels.iter().find(|label| label.label().contains("href=\"artist:")).expect("song artists must be links");
        let artist = ui.artist_profile.borrow().as_ref().unwrap().songs[0].artists[0].clone();
        artist_label.emit_by_name::<bool>("activate-link", &[&format!("artist:{}", artist.id)]);
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while ui.artist_loading.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ui.artist_profile.borrow().is_some());
        ui.artist_back.emit_clicked();
        assert!(ui.view.get() == View::Search);
        assert_eq!(ui.list.first_child().unwrap().downcast::<adw::ActionRow>().unwrap().title(), "Nujabes");
        assert!(!ui.artist_back.is_visible());
        ui.window.close();
        if directory.exists() { std::fs::remove_dir_all(directory).unwrap(); }
    }

    #[test]
    #[ignore = "requires a desktop display and live YouTube Music access"]
    fn ui_library_and_live_search() {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("io.github.oxidance.UiTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let directory = std::env::temp_dir().join(format!("oxidance-ui-test-{}", std::process::id()));
        let path = directory.join("library.json");
        let ui = Ui::new(&app, path.clone());
        pump();
        let song = Song { video_id: "example".into(), title: "A & B <song>".into(), artist: Some("Artist".into()), album_art_url: None, artists: vec![] };
        ui.results.borrow_mut().push(song.clone());
        ui.render();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while heart(&ui).width() == 0 && std::time::Instant::now() < deadline {
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let heart_button = heart(&ui);
        assert!(heart_button.width() > 0);
        assert_eq!(heart_button.width(), heart_button.height(), "heart must be square");
        assert_eq!(heart_button.allocated_width(), song_menu(&ui).allocated_width(), "heart and plus must have matching widths");
        heart(&ui).set_active(true);
        assert!(ui.library.borrow().is_liked(&song));
        assert!(Library::load(&path).unwrap().is_liked(&song));

        // Create a playlist through the actual song popover.
        let menu = song_menu(&ui);
        menu.popup();
        pump();
        let popover = menu.popover().unwrap();
        let widgets = descendants(popover.upcast_ref());
        let name = widgets.iter().find_map(|widget| widget.clone().downcast::<gtk::Entry>().ok()).unwrap();
        name.set_text("My <mix> & favorites");
        name.emit_by_name::<()>("activate", &[]);
        let first = ui.library.borrow().playlists[0].id;
        assert_eq!(ui.library.borrow().playlists[0].songs, vec![song.clone()]);
        assert!(ui.change(|library| library.create_playlist("Second").map(|_| ())));
        let second = ui.library.borrow().sorted_playlists()[0].id;
        assert_ne!(first, second);

        // Add through an existing-playlist choice, without duplicating the song.
        let menu = song_menu(&ui);
        menu.popup();
        pump();
        let popover = menu.popover().unwrap();
        let choice = descendants(popover.upcast_ref()).into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some("Second")).unwrap();
        choice.emit_clicked();
        assert_eq!(ui.library.borrow().playlists.iter().find(|playlist| playlist.id == second).unwrap().songs.len(), 1);
        assert_eq!(ui.library.borrow().sorted_playlists()[0].id, second);
        let sidebar_names: Vec<_> = descendants(ui.sidebar.upcast_ref()).into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .map(|button| button.widget_name().to_string())
            .filter(|name| name == "liked-songs" || name.starts_with("playlist-"))
            .collect();
        assert_eq!(sidebar_names[0], "liked-songs");
        assert_eq!(sidebar_names[1], format!("playlist-{second}"));
        assert_eq!(sidebar_names[2], format!("playlist-{first}"));

        ui.navigate(View::Liked);
        heart(&ui).set_active(false);
        assert!(ui.library.borrow().liked.is_empty());
        assert!(ui.list.first_child().is_none());
        ui.navigate(View::Playlist(first));
        let remove = descendants(ui.list.upcast_ref()).into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.tooltip_text().as_deref() == Some("Remove from this playlist")).unwrap();
        remove.emit_clicked();
        assert_eq!(ui.library.borrow().sorted_playlists()[0].id, first);
        assert!(ui.list.first_child().is_none());
        let saved = Library::load(&path).unwrap();
        assert_eq!(saved, *ui.library.borrow());
        let toggle = descendants(ui.window.upcast_ref()).into_iter()
            .filter_map(|widget| widget.downcast::<gtk::ToggleButton>().ok())
            .find(|button| button.tooltip_text().as_deref() == Some("Show or hide library sidebar")).unwrap();
        toggle.set_active(false);
        assert!(!ui.split.shows_sidebar());
        toggle.set_active(true);
        assert!(ui.split.shows_sidebar());

        // A library edit during a pending search must not discard its response.
        ui.entry.set_text("Daft Punk Get Lucky");
        ui.entry.emit_by_name::<()>("activate", &[]);
        assert!(ui.change(|library| library.create_playlist("During search").map(|_| ())));
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let mut loaded = 0;
        while std::time::Instant::now() < deadline {
            pump();
            loaded = descendants(ui.list.upcast_ref()).into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Image>().ok())
                .filter(|image| image.paintable().is_some()).count();
            if loaded > 0 && ui.results.borrow().len() == 25 { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ui.results.borrow().len(), 25);
        assert!(loaded > 0, "expected visible album covers to decode");
        assert!(!ui.pending_art.borrow().is_empty(), "offscreen artwork must wait until scrolled into view");
        assert!(!ui.page_loading.get());
        assert!(ui.cursor.borrow().is_some(), "expected a continuation for the next page");
        let first_id = ui.results.borrow()[0].video_id.clone();
        ui.scroll.vadjustment().set_value(ui.scroll.vadjustment().upper());
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while ui.results.borrow().len() < 50 && std::time::Instant::now() < deadline {
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ui.results.borrow().len(), 50);
        assert_eq!(ui.results.borrow()[0].video_id, first_id);
        let ids: std::collections::HashSet<_> = ui.results.borrow().iter().map(|song| song.video_id.clone()).collect();
        assert_eq!(ids.len(), 50, "pagination must not duplicate songs");
        assert!(ui.scroll.vadjustment().value() > 0.0, "pagination must preserve scroll position");

        ui.entry.set_text("Oasis");
        let deadline = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        ui.entry.set_text("Oasis Wonderwall");
        let deadline = std::time::Instant::now() + Duration::from_millis(900);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(*ui.query.borrow(), "Daft Punk Get Lucky", "typing must reset the one-second timer");
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        while (ui.query.borrow().as_str() != "Oasis Wonderwall" || ui.page_loading.get()) && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*ui.query.borrow(), "Oasis Wonderwall");
        assert_eq!(ui.results.borrow().len(), 25);
        ui.entry.set_text("");
        assert!(ui.results.borrow().is_empty());
        assert!(ui.cursor.borrow().is_none());
        assert!(ui.debounce.borrow().is_none());
        // Resolve a real song, decode its stream, and exercise the actual play/pause control.
        let stream_song = Song {
            video_id: "5NV6Rdv1a3I".into(), title: "Get Lucky".into(),
            artist: Some("Daft Punk".into()), album_art_url: Some("https://i.ytimg.com/vi/5NV6Rdv1a3I/hqdefault.jpg".into()), artists: vec![],
        };
        ui.play_song(&stream_song);
        let deadline = std::time::Instant::now() + Duration::from_secs(75);
        while std::time::Instant::now() < deadline {
            pump();
            if ui.player.borrow().as_ref().is_some_and(|player| player.current_state() == gst::State::Playing) { break; }
            if !ui.resolving.get() && ui.player.borrow().is_none() { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ui.player.borrow().as_ref().is_some_and(|player| player.current_state() == gst::State::Playing),
            "stream must reach Playing");
        ui.playback_button.emit_clicked();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            pump();
            if ui.player.borrow().as_ref().unwrap().current_state() == gst::State::Paused { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!ui.desired_playing.get());
        let player = ui.player.borrow().as_ref().unwrap().clone();
        assert_eq!(player.current_state(), gst::State::Paused);
        let paused_position = player.query_position::<gst::ClockTime>().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert_eq!(player.query_position::<gst::ClockTime>().unwrap(), paused_position);
        ui.update_timeline();
        assert!(ui.seek.is_sensitive(), "the audio stream should support seeking");
        assert!(ui.seek.adjustment().upper() > 30.0);
        ui.seek.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &30.0_f64]);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while ui.seek_pending.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(player.current_state(), gst::State::Paused, "seeking must preserve paused state");
        let paused_position = player.query_position::<gst::ClockTime>().unwrap();
        assert!((paused_position.nseconds() as f64 / 1e9 - 30.0).abs() < 0.5);
        ui.playback_button.emit_clicked();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pump();
            if player.query_position::<gst::ClockTime>().is_some_and(|position| position > paused_position + gst::ClockTime::from_mseconds(100)) { break; }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(player.query_position::<gst::ClockTime>().unwrap() > paused_position);
        ui.seek.emit_by_name::<bool>("change-value", &[&gtk::ScrollType::Jump, &5.0_f64]);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while ui.seek_pending.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        let position = player.query_position::<gst::ClockTime>().unwrap().nseconds() as f64 / 1e9;
        assert!((5.0..7.0).contains(&position), "expected playback near five seconds, got {position}");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while player.current_state() != gst::State::Playing && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        ui.navigate(View::Liked);
        assert_eq!(player.current_state(), gst::State::Playing, "navigation must not stop playback");
        ui.stop_player();
        assert_eq!(player.current_state(), gst::State::Null);
        assert!(!ui.seek.is_sensitive());
        // Download through the like action, then start playback without resolving a network URL.
        assert!(ui.change(|library| { *library = Library::default(); Ok(()) }));
        ui.start_download_queue();
        assert!(ui.change(|library| { library.toggle_like(&stream_song); Ok(()) }));
        assert!(ui.download_spinners.borrow().iter().any(|(id, weak)| id == &stream_song.video_id
            && weak.upgrade().is_some_and(|spinner| spinner.is_visible())),
            "queued downloads must show a spinner beside the play button");
        assert!(ui.change(|library| {
            let id = library.create_playlist("Downloaded song")?;
            library.add_song(id, &stream_song)?;
            Ok(())
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        while downloads::local_file(&ui.music_directory, &stream_song.video_id).is_none()
            && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        let local_path = downloads::local_file(&ui.music_directory, &stream_song.video_id).expect("saved song must download");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ui.download_queue.borrow().as_ref().unwrap().is_pending(&stream_song.video_id)
            && std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        pump();
        ui.update_download_spinners();
        assert!(downloads::local_art(&ui.music_directory, &stream_song.video_id).is_some(), "saved audio must also have offline artwork");
        let deadline = std::time::Instant::now() + Duration::from_millis(2100);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        assert!(ui.download_spinners.borrow().iter().filter(|(id, _)| id == &stream_song.video_id)
            .all(|(_, weak)| weak.upgrade().is_none_or(|spinner| !spinner.is_visible())));
        ui.play_song(&stream_song);
        assert!(!ui.resolving.get(), "downloaded songs must skip yt-dlp resolution");
        let local_player = ui.player.borrow().as_ref().unwrap().clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while local_player.current_state() != gst::State::Playing && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(local_player.current_state(), gst::State::Playing);
        assert_eq!(local_player.property::<Option<String>>("current-uri").as_deref(),
            Some(gtk::gio::File::for_path(&local_path).uri().as_str()));
        ui.playback_rate.set(2.0);
        ui.update_timeline();
        assert_eq!(ui.applied_rate.get(), 2.0);
        ui.seek_to(30.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ui.seek_pending.get() && std::time::Instant::now() < deadline {
            pump(); std::thread::sleep(Duration::from_millis(10));
        }
        ui.update_timeline();
        assert_eq!(ui.elapsed.text(), "0:15");
        let rate_start = local_player.query_position::<gst::ClockTime>().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < deadline { pump(); std::thread::sleep(Duration::from_millis(10)); }
        let advance = local_player.query_position::<gst::ClockTime>().unwrap() - rate_start;
        assert!(advance > gst::ClockTime::from_mseconds(400) && advance < gst::ClockTime::from_mseconds(850),
            "the resampled output timeline should advance at wall-clock speed");
        assert!(ui.change(|library| { library.toggle_like(&stream_song); Ok(()) }));
        assert!(local_path.exists(), "unliking must preserve the user's downloaded file");
        ui.stop_player();
        ui.window.close();
        let reopened = Ui::new(&app, path.clone());
        assert_eq!(*reopened.library.borrow(), Library::load(&path).unwrap());
        reopened.window.close();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
