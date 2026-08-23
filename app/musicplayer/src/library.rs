//! The track list.
//!
//! Deliberately not a tag reader. Parsing ID3, Vorbis comments and MP4 atoms is
//! a dependency and a week of edge cases, and this application exists to show a
//! React frontend driving a native renderer, not to be a media database. The
//! file stem is the title; anything more is a job for a real tag crate.

use std::path::{Path, PathBuf};

/// Extensions the bundled decoders can actually open.
///
/// Kept in step with rodio's default feature set. Listing a format that is
/// compiled out means the scan offers a track that fails the moment it is
/// clicked, which reads as a broken player rather than an absent codec.
const SUPPORTED: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "aac"];

/// One playable file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// Where the file is.
    pub path: PathBuf,
    /// What to show. The file stem, tidied.
    pub title: String,
    /// The containing folder's name, which is usually the album.
    pub album: String,
}

impl Track {
    /// Builds a track from a path, deriving what it can from the name.
    fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        if !SUPPORTED.contains(&extension.as_str()) {
            return None;
        }
        let title = path.file_stem()?.to_string_lossy().replace('_', " ").trim().to_owned();
        if title.is_empty() {
            return None;
        }
        let album = path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(Self { path: path.to_path_buf(), title, album })
    }
}

/// Every playable file under `root`, sorted.
///
/// Recurses, but not without limit: a symlink loop or a path that turns out to
/// be a mount point should not hang the application before its window has even
/// appeared.
pub fn scan(root: &Path) -> Vec<Track> {
    let mut tracks = Vec::new();
    collect(root, 0, &mut tracks);
    // Sorted by album then title so the same folder produces the same order
    // twice running; `read_dir` promises nothing about ordering.
    tracks.sort_by(|a, b| a.album.cmp(&b.album).then_with(|| a.title.cmp(&b.title)));
    tracks
}

/// How deep a scan will walk before giving up on a directory tree.
const MAX_DEPTH: usize = 6;

/// How many tracks a scan will collect.
///
/// A music folder can hold tens of thousands of files, and every one of them
/// crosses into JavaScript as JSON on every commit. The cap is a display limit,
/// not a storage one.
const MAX_TRACKS: usize = 500;

fn collect(directory: &Path, depth: usize, out: &mut Vec<Track>) {
    if depth > MAX_DEPTH || out.len() >= MAX_TRACKS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        if out.len() >= MAX_TRACKS {
            return;
        }
        let path = entry.path();
        // `file_type` rather than `is_dir`, because the latter follows symlinks
        // and that is exactly how a scan ends up walking in a circle.
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect(&path, depth + 1, out);
        } else if let Some(track) = Track::from_path(&path) {
            out.push(track);
        }
    }
}

/// How many playable files sit directly in `directory`, without recursing.
///
/// The folder tree calls this once per visible row, so it deliberately does not
/// walk: a recursive count on a drive root would stall the UI on every expand.
pub fn scan_shallow(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else { return 0 };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| Track::from_path(&entry.path()).is_some())
        .count()
}

/// The folder to scan when the application is started without an argument.
pub fn default_music_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join("Music"))
        .filter(|path| path.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("spherekit-musicplayer-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"").expect("write file");
    }

    #[test]
    fn only_formats_the_decoder_can_open_are_offered() {
        // Offering a track that cannot play looks like a broken player, not a
        // missing codec, so the filter is part of the user-visible behaviour.
        let root = temp_dir("formats");
        touch(&root.join("song.mp3"));
        touch(&root.join("song.flac"));
        touch(&root.join("notes.txt"));
        touch(&root.join("cover.jpg"));

        let titles: Vec<_> = scan(&root).into_iter().map(|t| t.title).collect();
        assert_eq!(titles, ["song", "song"]);
    }

    #[test]
    fn the_scan_recurses_and_takes_the_album_from_the_folder() {
        let root = temp_dir("recurse");
        touch(&root.join("Blue Train/01 Moments Notice.mp3"));
        touch(&root.join("Blue Train/02 Locomotion.mp3"));

        let tracks = scan(&root);
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().all(|t| t.album == "Blue Train"), "{tracks:#?}");
    }

    #[test]
    fn results_are_ordered_the_same_way_twice_running() {
        // `read_dir` gives no ordering guarantee, and an unstable playlist means
        // "next track" means something different on every launch.
        let root = temp_dir("order");
        for name in ["c.mp3", "a.mp3", "b.mp3"] {
            touch(&root.join(name));
        }
        let first: Vec<_> = scan(&root).into_iter().map(|t| t.title).collect();
        let second: Vec<_> = scan(&root).into_iter().map(|t| t.title).collect();
        assert_eq!(first, ["a", "b", "c"]);
        assert_eq!(first, second);
    }

    #[test]
    fn underscores_become_spaces_in_the_displayed_title() {
        let root = temp_dir("titles");
        touch(&root.join("Giant_Steps.mp3"));
        assert_eq!(scan(&root)[0].title, "Giant Steps");
    }

    #[test]
    fn a_missing_directory_is_an_empty_list_and_not_a_panic() {
        // The default music folder frequently does not exist. The application
        // has to open anyway and say so on screen.
        assert!(scan(Path::new("this/does/not/exist")).is_empty());
    }

    #[test]
    fn the_scan_stops_before_it_can_walk_forever() {
        // Nesting past the depth limit must terminate rather than recurse until
        // the stack runs out.
        let root = temp_dir("deep");
        let mut path = root.clone();
        for level in 0..(MAX_DEPTH + 4) {
            path = path.join(format!("level{level}"));
        }
        touch(&path.join("buried.mp3"));
        assert!(scan(&root).is_empty(), "the depth limit did not hold");
    }
}
