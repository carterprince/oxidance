use gtk::gio;

pub fn load() -> gio::Settings {
    let directory = std::path::PathBuf::from(env!("OUT_DIR"));
    #[cfg(not(test))]
    let directory = {
        let installed = gtk::glib::user_data_dir().join("oxidance/schemas");
        if directory.join("gschemas.compiled").is_file() { directory } else { installed }
    };
    let source = gio::SettingsSchemaSource::from_directory(directory,
        gio::SettingsSchemaSource::default().as_ref(), false).expect("Compiled settings schema must be available");
    let schema = source.lookup("io.github.oxidance", false).expect("Playback settings schema must exist");
    #[cfg(test)]
    let backend = Some(gio::memory_settings_backend_new());
    #[cfg(not(test))]
    let backend: Option<gio::SettingsBackend> = None;
    gio::Settings::new_full(&schema, backend.as_ref(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::*;
    #[test]
    fn speed_settings_round_trip_and_enforce_range() {
        let settings = load();
        assert_eq!(settings.double("playback-speed"), 1.0);
        settings.set_double("playback-speed", 1.23).unwrap();
        let reopened = gio::Settings::new_full(&settings.settings_schema().unwrap(), settings.backend().as_ref(), None);
        assert_eq!(reopened.double("playback-speed"), 1.23);
        assert!(!settings.boolean("artwork-in-sidebar"));
        settings.set_boolean("artwork-in-sidebar", true).unwrap();
        assert!(reopened.boolean("artwork-in-sidebar"));
        assert!(!settings.boolean("shuffle"));
        assert_eq!(settings.string("last-song"), "");
        settings.set_string("last-song", "saved metadata").unwrap();
        assert_eq!(reopened.string("last-song"), "saved metadata");
        settings.set_boolean("shuffle", true).unwrap();
        assert!(reopened.boolean("shuffle"));
        reopened.set_boolean("shuffle", false).unwrap();
        assert!(!settings.boolean("shuffle"));
        assert!(!settings.settings_schema().unwrap().key("playback-speed").range_check(&2.01.to_variant()));
    }
}
