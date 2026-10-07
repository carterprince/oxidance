use std::{fs, io, path::Path, time::{SystemTime, UNIX_EPOCH}};

use serde::{Deserialize, Serialize};
use crate::Song;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Library {
    pub liked: Vec<Song>,
    pub playlists: Vec<Playlist>,
    revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Playlist {
    pub id: u64,
    pub name: String,
    pub modified_at: u64,
    pub songs: Vec<Song>,
}

impl Library {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| io::Error::other("Missing library directory"))?;
        fs::create_dir_all(parent)?;
        // Replace atomically so a partial write does not destroy the saved library.
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut file = fs::File::create(&temporary)?;
        use io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    }

    fn tick(&mut self) -> u64 {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        self.revision = now.max(self.revision + 1);
        self.revision
    }

    pub fn is_liked(&self, song: &Song) -> bool {
        self.liked.iter().any(|saved| saved.video_id == song.video_id)
    }

    pub fn toggle_like(&mut self, song: &Song) {
        if self.is_liked(song) {
            self.liked.retain(|saved| saved.video_id != song.video_id);
        } else {
            self.liked.insert(0, song.clone());
        }
    }

    pub fn create_playlist(&mut self, name: &str) -> Result<u64, String> {
        let name = name.trim();
        if name.is_empty() { return Err("Enter a playlist name.".into()); }
        if self.playlists.iter().any(|playlist| playlist.name.to_lowercase() == name.to_lowercase()) {
            return Err("A playlist with that name already exists.".into());
        }
        let id = self.tick();
        self.playlists.push(Playlist { id, name: name.to_owned(), modified_at: id, songs: vec![] });
        Ok(id)
    }

    pub fn add_song(&mut self, playlist_id: u64, song: &Song) -> Result<bool, String> {
        let index = self.playlists.iter().position(|playlist| playlist.id == playlist_id)
            .ok_or("Playlist no longer exists.")?;
        if self.playlists[index].songs.iter().any(|saved| saved.video_id == song.video_id) {
            return Ok(false);
        }
        let modified = self.tick();
        self.playlists[index].songs.push(song.clone());
        self.playlists[index].modified_at = modified;
        Ok(true)
    }

    pub fn remove_song(&mut self, playlist_id: u64, song: &Song) -> Result<(), String> {
        let index = self.playlists.iter().position(|playlist| playlist.id == playlist_id)
            .ok_or("Playlist no longer exists.")?;
        if self.playlists[index].songs.iter().any(|saved| saved.video_id == song.video_id) {
            let modified = self.tick();
            self.playlists[index].songs.retain(|saved| saved.video_id != song.video_id);
            self.playlists[index].modified_at = modified;
        }
        Ok(())
    }

    pub fn rename_playlist(&mut self, id: u64, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() { return Err("Enter a playlist name.".into()); }
        if self.playlists.iter().any(|p| p.id != id && p.name.to_lowercase() == name.to_lowercase()) {
            return Err("A playlist with that name already exists.".into());
        }
        let index = self.playlists.iter().position(|p| p.id == id).ok_or("Playlist no longer exists.")?;
        if self.playlists[index].name != name {
            let modified = self.tick();
            self.playlists[index].name = name.to_owned();
            self.playlists[index].modified_at = modified;
        }
        Ok(())
    }

    pub fn delete_playlist(&mut self, id: u64) -> Result<(), String> {
        let index = self.playlists.iter().position(|p| p.id == id).ok_or("Playlist no longer exists.")?;
        self.playlists.remove(index);
        Ok(())
    }

    pub fn sorted_playlists(&self) -> Vec<&Playlist> {
        let mut playlists: Vec<_> = self.playlists.iter().collect();
        playlists.sort_by_key(|playlist| std::cmp::Reverse(playlist.modified_at));
        playlists
    }

    pub fn is_empty(&self) -> bool { self.liked.is_empty() && self.playlists.is_empty() }

    /// Three-way merge of two libraries that diverged from `base`, the last synced state.
    /// Without a base, both sides are combined and nothing is treated as deleted.
    /// Removals on either side win over unchanged entries; additions on both sides are kept.
    pub fn merge(base: Option<&Library>, local: &Library, remote: &Library) -> Library {
        let empty = Library::default();
        let base = base.unwrap_or(&empty);
        let mut playlists = Vec::new();
        let ids = local.playlists.iter().chain(&remote.playlists).map(|playlist| playlist.id);
        for id in ids {
            if playlists.iter().any(|playlist: &Playlist| playlist.id == id) { continue; }
            fn find(library: &Library, id: u64) -> Option<&Playlist> { library.playlists.iter().find(|playlist| playlist.id == id) }
            match (find(base, id), find(local, id), find(remote, id)) {
                (Some(_), None, _) | (Some(_), _, None) => {} // Deleted on one side.
                (None, Some(playlist), None) | (None, None, Some(playlist)) => playlists.push(playlist.clone()),
                (base, Some(local), Some(remote)) => {
                    let local_renamed = base.is_none_or(|base| base.name != local.name);
                    let remote_renamed = base.is_none_or(|base| base.name != remote.name);
                    let name = if remote_renamed && (!local_renamed || remote.modified_at > local.modified_at) {
                        &remote.name
                    } else { &local.name };
                    playlists.push(Playlist {
                        id,
                        name: name.clone(),
                        modified_at: local.modified_at.max(remote.modified_at),
                        songs: merge_songs(base.map_or(&[], |base| &base.songs), &local.songs, &remote.songs),
                    });
                }
                (_, None, None) => unreachable!(),
            }
        }
        // Playlists created separately on two devices can share a name; keep names unique.
        playlists.sort_by_key(|playlist| playlist.id);
        for index in 0..playlists.len() {
            let taken = |name: &str, playlists: &[Playlist]| playlists.iter().any(|other| other.name.to_lowercase() == name.to_lowercase());
            if !taken(&playlists[index].name, &playlists[..index]) { continue; }
            let original = playlists[index].name.clone();
            let name = (2..).map(|number| format!("{original} ({number})"))
                .find(|name| !taken(name, &playlists)).unwrap();
            playlists[index].name = name;
        }
        Library {
            liked: merge_songs(&base.liked, &local.liked, &remote.liked),
            playlists,
            revision: local.revision.max(remote.revision),
        }
    }
}

fn merge_songs(base: &[Song], local: &[Song], remote: &[Song]) -> Vec<Song> {
    let contains = |list: &[Song], id: &str| list.iter().any(|song| song.video_id == id);
    let keep = |song: &Song| !contains(base, &song.video_id)
        || contains(local, &song.video_id) && contains(remote, &song.video_id);
    let mut merged: Vec<Song> = remote.iter().filter(|song| keep(song))
        .map(|song| local.iter().find(|saved| saved.video_id == song.video_id).unwrap_or(song).clone())
        .collect();
    // Place local additions next to their nearest local neighbour that is kept.
    for (index, song) in local.iter().enumerate() {
        if contains(&merged, &song.video_id) || !keep(song) { continue; }
        let find = |other: &Song| merged.iter().position(|saved| saved.video_id == other.video_id);
        let position = local[..index].iter().rev().find_map(find).map(|position| position + 1)
            .or_else(|| local[index + 1..].iter().find_map(find))
            .unwrap_or(merged.len());
        merged.insert(position, song.clone());
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song { video_id: id.into(), title: "Same title".into(), artist: None, album_art_url: None, artists: vec![] }
    }

    #[test]
    fn likes_and_playlist_membership_use_video_id() {
        let mut library = Library::default();
        library.toggle_like(&song("one"));
        library.toggle_like(&song("two"));
        library.toggle_like(&song("one"));
        assert!(!library.is_liked(&song("one")));
        assert!(library.is_liked(&song("two")));
        let id = library.create_playlist(" Mix ").unwrap();
        assert!(library.add_song(id, &song("one")).unwrap());
        assert!(!library.add_song(id, &song("one")).unwrap());
        assert!(library.add_song(id, &song("two")).unwrap());
        assert_eq!(library.playlists[0].songs.len(), 2);
        assert!(library.create_playlist("mix").is_err());
        assert!(library.create_playlist(" ").is_err());
    }

    #[test]
    fn actual_modifications_reorder_playlists() {
        let mut library = Library::default();
        let first = library.create_playlist("First").unwrap();
        let second = library.create_playlist("Second").unwrap();
        assert_eq!(library.sorted_playlists()[0].id, second);
        library.add_song(first, &song("one")).unwrap();
        assert_eq!(library.sorted_playlists()[0].id, first);
        library.add_song(second, &song("two")).unwrap();
        library.add_song(first, &song("one")).unwrap();
        assert_eq!(library.sorted_playlists()[0].id, second, "duplicate adds must not modify ordering");
        library.remove_song(first, &song("one")).unwrap();
        assert_eq!(library.sorted_playlists()[0].id, first);
    }

    #[test]
    fn rename_and_delete_preserve_other_collections() {
        let mut library = Library::default();
        let first = library.create_playlist("First").unwrap();
        let second = library.create_playlist("Second").unwrap();
        library.add_song(first, &song("one")).unwrap();
        library.toggle_like(&song("one"));
        assert!(library.rename_playlist(first, "second").is_err());
        assert!(library.rename_playlist(first, " ").is_err());
        library.rename_playlist(first, " Renamed ").unwrap();
        assert_eq!(library.sorted_playlists()[0].name, "Renamed");
        assert_eq!(library.sorted_playlists()[0].songs.len(), 1);
        library.delete_playlist(first).unwrap();
        assert_eq!(library.playlists.len(), 1);
        assert_eq!(library.playlists[0].id, second);
        assert!(library.is_liked(&song("one")));
        assert!(library.delete_playlist(first).is_err());
    }

    fn ids(songs: &[Song]) -> Vec<&str> { songs.iter().map(|song| song.video_id.as_str()).collect() }

    #[test]
    fn merge_without_base_combines_both_sides() {
        let mut local = Library::default();
        local.toggle_like(&song("a"));
        local.toggle_like(&song("b"));
        let mut remote = Library::default();
        remote.toggle_like(&song("c"));
        remote.toggle_like(&song("b"));
        let merged = Library::merge(None, &local, &remote);
        assert_eq!(ids(&merged.liked), ["b", "a", "c"]);
        assert_eq!(Library::merge(None, &merged, &Library::default()), merged);
    }

    #[test]
    fn merge_applies_removals_and_additions_from_both_sides() {
        let mut base = Library::default();
        for id in ["a", "b", "c"] { base.toggle_like(&song(id)); }
        let mut local = base.clone();
        local.toggle_like(&song("a"));
        local.toggle_like(&song("new-local"));
        let mut remote = base.clone();
        remote.toggle_like(&song("c"));
        remote.toggle_like(&song("new-remote"));
        let merged = Library::merge(Some(&base), &local, &remote);
        assert_eq!(ids(&merged.liked), ["new-remote", "new-local", "b"]);
        // Merging again with the result as base is stable.
        assert_eq!(Library::merge(Some(&merged), &merged, &merged), merged);
    }

    #[test]
    fn merge_playlists_by_id() {
        let mut base = Library::default();
        let kept = base.create_playlist("Kept").unwrap();
        let deleted = base.create_playlist("Deleted").unwrap();
        base.add_song(kept, &song("a")).unwrap();
        base.add_song(deleted, &song("a")).unwrap();
        let mut local = base.clone();
        local.add_song(kept, &song("local")).unwrap();
        local.add_song(deleted, &song("local")).unwrap();
        local.rename_playlist(kept, "Renamed").unwrap();
        let local_only = local.create_playlist("Local only").unwrap();
        let mut remote = base.clone();
        remote.add_song(kept, &song("remote")).unwrap();
        remote.remove_song(kept, &song("a")).unwrap();
        remote.delete_playlist(deleted).unwrap();
        let merged = Library::merge(Some(&base), &local, &remote);
        assert_eq!(merged.playlists.len(), 2, "deletion wins over additions elsewhere");
        let playlist = merged.playlists.iter().find(|playlist| playlist.id == kept).unwrap();
        assert_eq!(playlist.name, "Renamed");
        assert_eq!(ids(&playlist.songs), ["remote", "local"]);
        assert!(merged.playlists.iter().any(|playlist| playlist.id == local_only));
    }

    #[test]
    fn merge_resolves_concurrent_renames_and_duplicate_names() {
        let mut base = Library::default();
        let id = base.create_playlist("Mix").unwrap();
        let mut local = base.clone();
        local.rename_playlist(id, "Local name").unwrap();
        let mut remote = base.clone();
        remote.rename_playlist(id, "Remote name").unwrap();
        remote.playlists[0].modified_at = local.playlists[0].modified_at + 1;
        let merged = Library::merge(Some(&base), &local, &remote);
        assert_eq!(merged.playlists[0].name, "Remote name", "the later rename wins");
        let mut first = Library::default();
        first.create_playlist("Road trip").unwrap();
        let mut second = Library::default();
        second.create_playlist("road trip").unwrap();
        second.playlists[0].id += 1;
        let merged = Library::merge(None, &first, &second);
        let mut names: Vec<_> = merged.playlists.iter().map(|playlist| playlist.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Road trip", "road trip (2)"]);
    }

    #[test]
    fn library_round_trip_and_corrupt_data_detection() {
        let directory = std::env::temp_dir().join(format!("oxidance-library-test-{}", std::process::id()));
        let path = directory.join("library.json");
        assert_eq!(Library::load(&path).unwrap(), Library::default());
        let mut library = Library::default();
        library.toggle_like(&song("one"));
        let id = library.create_playlist("Saved").unwrap();
        library.add_song(id, &song("one")).unwrap();
        library.save(&path).unwrap();
        assert_eq!(Library::load(&path).unwrap(), library);
        fs::write(&path, "invalid json").unwrap();
        assert!(Library::load(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}
