use super::*;
use gtk::gio;
use glib::variant::ObjectPath;
use std::collections::BTreeMap;

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
pub fn bus_name() -> String {
    #[cfg(test)]
    return format!("org.mpris.MediaPlayer2.Oxidance.test{}", std::process::id());
    #[cfg(not(test))]
    "org.mpris.MediaPlayer2.Oxidance".to_owned()
}
const XML: &str = r#"<node>
<interface name="org.mpris.MediaPlayer2">
<method name="Raise"/><method name="Quit"/>
<property name="CanQuit" type="b" access="read"/><property name="CanRaise" type="b" access="read"/>
<property name="HasTrackList" type="b" access="read"/><property name="Identity" type="s" access="read"/>
<property name="DesktopEntry" type="s" access="read"/>
<property name="SupportedUriSchemes" type="as" access="read"/><property name="SupportedMimeTypes" type="as" access="read"/>
</interface>
<interface name="org.mpris.MediaPlayer2.Player">
<method name="Play"/><method name="Pause"/><method name="PlayPause"/><method name="Stop"/><method name="Next"/><method name="Previous"/>
<method name="Seek"><arg name="Offset" type="x" direction="in"/></method>
<method name="SetPosition"><arg name="TrackId" type="o" direction="in"/><arg name="Position" type="x" direction="in"/></method>
<method name="OpenUri"><arg name="Uri" type="s" direction="in"/></method>
<signal name="Seeked"><arg name="Position" type="x"/></signal>
<property name="PlaybackStatus" type="s" access="read"/><property name="Rate" type="d" access="readwrite"/>
<property name="Shuffle" type="b" access="readwrite"/>
<property name="Metadata" type="a{sv}" access="read"/><property name="Volume" type="d" access="readwrite"/>
<property name="Position" type="x" access="read"/><property name="MinimumRate" type="d" access="read"/><property name="MaximumRate" type="d" access="read"/>
<property name="CanGoNext" type="b" access="read"/><property name="CanGoPrevious" type="b" access="read"/>
<property name="CanPlay" type="b" access="read"/><property name="CanPause" type="b" access="read"/>
<property name="CanSeek" type="b" access="read"/><property name="CanControl" type="b" access="read"/>
</interface></node>"#;

pub struct Service {
    connection: gio::DBusConnection,
    registrations: Vec<gio::RegistrationId>,
    owner: Option<gio::OwnerId>,
    last: RefCell<BTreeMap<String, glib::Variant>>,
}

fn track_id(ui: &Ui) -> ObjectPath {
    let id = ui.current_song.borrow().as_ref().map(|song| song.video_id.bytes()
        .map(|byte| format!("{byte:02x}")).collect::<String>());
    ObjectPath::try_from(id.map(|id| format!("{PATH}/track/{id}")).unwrap_or_else(|| format!("{PATH}/TrackList/NoTrack"))).unwrap()
}

fn position(ui: &Ui) -> i64 {
    ui.player.borrow().as_ref().and_then(|player| player.query_position::<gst::ClockTime>())
        .map(|time| (time.useconds() as f64 * ui.applied_rate.get()) as i64).unwrap_or(0)
}

fn properties(ui: &Ui) -> BTreeMap<String, glib::Variant> {
    let mut metadata = BTreeMap::<String, glib::Variant>::new();
    if let Some(song) = ui.current_song.borrow().as_ref() {
        metadata.insert("mpris:trackid".into(), track_id(ui).to_variant());
        metadata.insert("xesam:title".into(), song.title.to_variant());
        let artists: Vec<String> = if song.artists.is_empty() { song.artist.iter().cloned().collect() }
            else { song.artists.iter().map(|artist| artist.name.clone()).collect() };
        metadata.insert("xesam:artist".into(), artists.to_variant());
        metadata.insert("xesam:url".into(), song.page_url().to_variant());
        let art = downloads::local_art(&ui.music_directory, &song.video_id)
            .map(|path| gio::File::for_path(path).uri().to_string())
            .or_else(|| song.album_art_url.as_deref().map(oxidance::high_quality_art_url));
        if let Some(art) = art { metadata.insert("mpris:artUrl".into(), art.to_variant()); }
        if let Some(duration) = ui.player.borrow().as_ref().and_then(|player| player.query_duration::<gst::ClockTime>()) {
            metadata.insert("mpris:length".into(), ((duration.useconds() as f64 * ui.applied_rate.get()) as i64).to_variant());
        }
    }
    let active = ui.player.borrow().is_some() || ui.resolving.get();
    BTreeMap::from([
        ("PlaybackStatus".into(), (if !active { "Stopped" } else if ui.desired_playing.get() { "Playing" } else { "Paused" }).to_variant()),
        ("Metadata".into(), metadata.to_variant()),
        ("Rate".into(), ui.applied_rate.get().to_variant()),
        ("Volume".into(), ui.volume.get().to_variant()),
        ("Shuffle".into(), ui.shuffle_button.is_active().to_variant()),
        ("MinimumRate".into(), 0.5_f64.to_variant()), ("MaximumRate".into(), 2.0_f64.to_variant()),
        ("CanGoNext".into(), (ui.queue_position.get() + 1 < ui.playback_order.borrow().len()).to_variant()),
        ("CanGoPrevious".into(), (ui.queue_position.get() > 0).to_variant()),
        ("CanPlay".into(), ui.current_song.borrow().is_some().to_variant()),
        ("CanPause".into(), active.to_variant()), ("CanSeek".into(), ui.seek.is_sensitive().to_variant()),
        ("CanControl".into(), true.to_variant()),
    ])
}

impl Service {
    pub fn new(ui: &Rc<Ui>) -> Result<Self, glib::Error> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
        let info = gio::DBusNodeInfo::for_xml(XML)?;
        let mut registrations = Vec::new();
        for interface in ["org.mpris.MediaPlayer2", PLAYER] {
            let weak = Rc::downgrade(ui);
            let property_ui = weak.clone();
            let setter_ui = weak.clone();
            let registration = connection.register_object(PATH, &info.lookup_interface(interface).unwrap())
                .method_call(move |_, _, _, _, method, parameters, invocation| {
                    let Some(ui) = weak.upgrade() else { invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", "Player closed"); return; };
                    match method {
                        "Raise" => ui.window.present(),
                        "Quit" => { invocation.return_value(Some(&().to_variant())); ui.window.close(); return; }
                        "PlayPause" => ui.toggle_playback(),
                        "Next" => ui.advance_queue(true), "Previous" => ui.advance_queue(false),
                        "Play" if !ui.desired_playing.get() => ui.toggle_playback(),
                        "Pause" if ui.desired_playing.get() => ui.toggle_playback(),
                        "Stop" => { ui.stop_player(); ui.desired_playing.set(false); ui.update_playback_buttons(); }
                        "Seek" => if let Some((offset,)) = parameters.get::<(i64,)>() {
                            ui.seek_to((position(&ui).saturating_add(offset) as f64 / 1_000_000.0).max(0.0));
                        },
                        "SetPosition" => if let Some((id, microseconds)) = parameters.get::<(ObjectPath, i64)>() {
                            let seconds = microseconds as f64 / 1_000_000.0;
                            if id == track_id(&ui) && microseconds >= 0 && seconds <= ui.seek.adjustment().upper() { ui.seek_to(seconds); }
                        },
                        "OpenUri" => { invocation.return_dbus_error("org.freedesktop.DBus.Error.NotSupported", "OpenUri is not supported"); return; }
                        _ => (),
                    }
                    invocation.return_value(Some(&().to_variant()));
                })
                .property(move |_, _, _, interface, property| {
                    let ui = property_ui.upgrade().unwrap();
                    if interface == PLAYER {
                        if property == "Position" { return position(&ui).to_variant(); }
                        return properties(&ui).remove(property).unwrap();
                    }
                    match property {
                        "Identity" => "Oxidance".to_variant(), "DesktopEntry" => "io.github.oxidance.Oxidance".to_variant(),
                        "CanQuit" | "CanRaise" => true.to_variant(), "HasTrackList" => false.to_variant(),
                        _ => Vec::<String>::new().to_variant(),
                    }
                })
                .set_property(move |_, _, _, _, property, value| {
                    let Some(ui) = setter_ui.upgrade() else { return false; };
                    if property == "Shuffle" {
                        if let Some(value) = value.get::<bool>() { ui.shuffle_button.set_active(value); return true; }
                        return false;
                    }
                    let Some(value) = value.get::<f64>() else { return false; };
                    if !value.is_finite() { return false; }
                    match property {
                        "Rate" if value == 0.0 => { if ui.desired_playing.get() { ui.toggle_playback(); } }
                        "Rate" if (0.5..=2.0).contains(&value) => { let _ = ui.settings.set_double("playback-speed", value); }
                        "Volume" => { ui.volume.set(value.max(0.0)); if let Some(player) = ui.player.borrow().as_ref() { player.set_property("volume", value.max(0.0)); } }
                        _ => return false,
                    }
                    true
                }).build()?;
            registrations.push(registration);
        }
        let owner = gio::bus_own_name_on_connection(&connection, &bus_name(),
            gio::BusNameOwnerFlags::NONE, |_, _| {}, |_, _| {});
        Ok(Self { connection, registrations, owner: Some(owner), last: RefCell::new(BTreeMap::new()) })
    }

    pub fn update(&self, ui: &Ui) {
        let properties = properties(ui);
        let changes: BTreeMap<_, _> = properties.iter().filter(|(key, value)| self.last.borrow().get(*key) != Some(*value))
            .map(|(key, value)| (key.clone(), value.clone())).collect();
        if !changes.is_empty() {
            let _ = self.connection.emit_signal(None, PATH, "org.freedesktop.DBus.Properties", "PropertiesChanged",
                Some(&(PLAYER, changes, Vec::<String>::new()).to_variant()));
            *self.last.borrow_mut() = properties;
        }
    }

    pub fn seeked(&self, ui: &Ui) {
        let _ = self.connection.emit_signal(None, PATH, PLAYER, "Seeked", Some(&(position(ui),).to_variant()));
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) { let _ = self.connection.unregister_object(registration); }
        if let Some(owner) = self.owner.take() { gio::bus_unown_name(owner); }
    }
}
