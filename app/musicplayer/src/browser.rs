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
/// Reports the folder itself, its parent (so the UI can offer "up") and its
/// immediate subfolders. Whether a folder holds playable audio is answered
/// here too, because the alternative is React asking per row.
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
        "truncated": truncated,
        "trackCount": crate::library::scan_shallow(path),
    })
}

/// Where the tree starts when nothing else is known.
///
/// The music folder first, because that is what a music player should open on;
/// the drives behind it, because a library that lives anywhere else has to be
/// reachable without typing a path.
pub fn roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(music) = crate::library::default_music_directory() {
        roots.push(music);
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = PathBuf::from(home);
        if home.is_dir() && !roots.contains(&home) {
            roots.push(home);
        }
    }
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        let drive = PathBuf::from(format!("{}:\\", letter as char));
        if drive.is_dir() {
            roots.push(drive);
        }
    }
    #[cfg(not(windows))]
    roots.push(PathBuf::from("/"));
    roots
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
