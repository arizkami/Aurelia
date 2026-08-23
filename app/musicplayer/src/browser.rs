//! The folder tree behind the sidebar.
//!
//! Rust owns the filesystem and answers one level at a time; React owns which
//! nodes are open. Splitting it that way means a tree of any depth costs one
//! `readdir` per expanded folder, instead of a full recursive walk serialised
//! into the commit whether or not anyone looks at it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Folders shown per level.
///
/// A directory with tens of thousands of entries would otherwise cross the
/// bridge in full and land in a React list nobody can scroll.
const MAX_ENTRIES: usize = 400;

/// One level of the tree, as JSON.
///
/// Reports the folder itself, its parent, its immediate subfolders, and the
/// playable files directly inside it. Everything else in the directory is left
/// out: a music browser that lists `.dll` and `.txt` is a file manager, and the
/// player cannot open them anyway.
pub fn list(path: &Path) -> Value {
    let mut folders = Vec::new();
    let mut truncated = false;

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if folders.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            // `file_type` rather than `is_dir`: the latter follows symlinks,
            // and a link back up the tree is how a browser starts recursing
            // into itself.
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden and system folders are noise in a music browser, and on
            // Windows the profile root is full of them.
            if name.starts_with('.') || name.starts_with('$') {
                continue;
            }
            folders.push(json!({
                "name": name,
                "path": entry.path().to_string_lossy(),
            }));
        }
    }

    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if files.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_file() {
                continue;
            }
            let file = entry.path();
            // Only what the bundled decoders can open. Offering anything else
            // means a row that fails the moment it is clicked.
            let Some(track) = crate::library::track_at(&file) else { continue };
            files.push(json!({
                "name": track.title,
                "path": file.to_string_lossy(),
            }));
        }
    }
    files.sort_by(|a, b| {
        let left = a["name"].as_str().unwrap_or_default().to_lowercase();
        let right = b["name"].as_str().unwrap_or_default().to_lowercase();
        left.cmp(&right)
    });

    folders.sort_by(|a, b| {
        let left = a["name"].as_str().unwrap_or_default().to_lowercase();
        let right = b["name"].as_str().unwrap_or_default().to_lowercase();
        left.cmp(&right)
    });

    json!({
        "path": path.to_string_lossy(),
        "name": display_name(path),
        "parent": path.parent().map(|parent| parent.to_string_lossy().into_owned()),
        "folders": folders,
        "files": files,
        "truncated": truncated,
        "trackCount": crate::library::scan_shallow(path),
    })
}

/// Where the tree starts: the drives themselves, and nothing else.
///
/// Shortcuts to the music and home folders used to sit above these. They are
/// gone deliberately — they duplicate paths already reachable by expanding a
/// drive, and a tree whose first level mixes "a place" with "a device" makes
/// the same folder appear twice under different names.
pub fn roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
            .filter(|drive| drive.is_dir())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

/// The roots, as JSON, for the tree's top level.
pub fn roots_json() -> Value {
    Value::Array(
        roots()
            .into_iter()
            .map(|path| json!({ "name": display_name(&path), "path": path.to_string_lossy() }))
            .collect(),
    )
}

/// A human label for a path, falling back to the path itself for a drive root.
///
/// `C:\` has no file name, and an empty label in a tree is a row you cannot
/// see or click.
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("spherekit-browser-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn the_tree_starts_at_the_drives_and_nothing_else() {
        // Shortcuts to Music and Home used to sit above the drives, which made
        // the same folder appear twice under two different names.
        let roots = roots();
        assert!(!roots.is_empty(), "no roots at all");
        for root in &roots {
            assert!(root.parent().is_none(), "{} is not a drive root", root.display());
        }
    }

    #[test]
    fn a_level_lists_playable_files_and_nothing_else() {
        // The tree is a music browser. Listing `.dll` and `.txt` would make it
        // a file manager, and the player cannot open them anyway.
        let root = temp_dir("files");
        std::fs::write(root.join("song.mp3"), b"").unwrap();
        std::fs::write(root.join("other.flac"), b"").unwrap();
        std::fs::write(root.join("readme.txt"), b"").unwrap();
        std::fs::write(root.join("art.jpg"), b"").unwrap();

        let level = list(&root);
        let names: Vec<_> = level["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["other", "song"], "the tree listed something unplayable");
    }

    #[test]
    fn listed_files_carry_the_path_needed_to_play_them() {
        let root = temp_dir("filepaths");
        std::fs::write(root.join("song.mp3"), b"").unwrap();
        let level = list(&root);
        let path = level["files"][0]["path"].as_str().unwrap();
        assert!(path.ends_with("song.mp3"), "{path}");
    }

    #[test]
    fn a_level_reports_only_its_immediate_subfolders() {
        // One `readdir` per expanded node is the whole point; returning
        // grandchildren would make the tree cost a full walk on every click.
        let root = temp_dir("levels");
        std::fs::create_dir_all(root.join("Albums/Blue Train")).unwrap();
        std::fs::create_dir_all(root.join("Singles")).unwrap();

        let level = list(&root);
        let names: Vec<_> =
            level["folders"].as_array().unwrap().iter().map(|f| f["name"].clone()).collect();
        assert_eq!(names, [json!("Albums"), json!("Singles")]);
    }

    #[test]
    fn folders_are_sorted_case_insensitively() {
        // `read_dir` gives no order, and an ASCII sort puts every lower-case
        // folder below every upper-case one, which reads as random.
        let root = temp_dir("sorting");
        for name in ["zeta", "Alpha", "beta"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let level = list(&root);
        let names: Vec<_> = level["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["Alpha", "beta", "zeta"]);
    }

    #[test]
    fn hidden_and_system_folders_are_left_out() {
        let root = temp_dir("hidden");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("$RECYCLE.BIN")).unwrap();
        std::fs::create_dir_all(root.join("Music")).unwrap();

        let level = list(&root);
        assert_eq!(level["folders"].as_array().map(Vec::len), Some(1));
        assert_eq!(level["folders"][0]["name"], json!("Music"));
    }

    #[test]
    fn a_level_says_how_many_playable_files_it_holds() {
        // The count is what tells a user which folder is worth opening, and
        // computing it here saves React a call per row.
        let root = temp_dir("counts");
        std::fs::write(root.join("a.mp3"), b"").unwrap();
        std::fs::write(root.join("b.flac"), b"").unwrap();
        std::fs::write(root.join("notes.txt"), b"").unwrap();
        assert_eq!(list(&root)["trackCount"], json!(2));
    }

    #[test]
    fn an_unreadable_directory_is_an_empty_level_and_not_a_panic() {
        // Permission-denied folders are everywhere under a Windows profile.
        let level = list(Path::new("this/does/not/exist"));
        assert_eq!(level["folders"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn a_drive_root_still_has_a_label() {
        // `C:\` has no file name; an empty label is an invisible tree row.
        assert!(!display_name(Path::new("C:\\")).is_empty());
        assert_eq!(display_name(Path::new("/tmp/Music")), "Music");
    }

    #[test]
    fn the_parent_is_reported_so_the_tree_can_walk_up() {
        let root = temp_dir("parent");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let level = list(&child);
        assert_eq!(level["parent"], json!(root.to_string_lossy()));
    }
}
