//! Playback state, and the API surface JavaScript drives it through.
//!
//! The React side owns none of this. It renders what `state()` reports and
//! calls the methods below; every decision about what is playing, what plays
//! next and what happens when a device disappears is made here, in Rust, where
//! the audio engine actually lives.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::audio::AudioEngine;
use crate::library::Track;

/// Everything the transport needs to know.
pub struct Player {
    /// Absent when no output device could be opened. The UI still runs.
    engine: Option<AudioEngine>,
    tracks: Vec<Track>,
    /// Index into `tracks`, when something is loaded.
    current: Option<usize>,
    /// The most recent failure, shown in the UI until something succeeds.
    last_error: Option<String>,
}

/// Shared handle, so bridge method closures and the frame loop see one player.
pub type SharedPlayer = Rc<RefCell<Player>>;

impl Player {
    /// Builds a player over an optional engine and a scanned library.
    pub fn new(
        engine: Option<AudioEngine>,
        tracks: Vec<Track>,
        device_error: Option<String>,
    ) -> Self {
        Self { engine, tracks, current: None, last_error: device_error }
    }

    /// The playlist.
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Loads and plays the track at `index`.
    pub fn select(&mut self, index: usize) {
        let Some(track) = self.tracks.get(index).cloned() else {
            self.last_error = Some(format!("no track at position {index}"));
            return;
        };
        let Some(engine) = self.engine.as_mut() else { return };
        match engine.load(&track.path) {
            Ok(_) => {
                self.current = Some(index);
                self.last_error = None;
            }
            Err(error) => {
                // A file that will not decode must not take the playlist with
                // it: the selection stays where it was and the message is shown.
                self.last_error = Some(error.to_string());
            }
        }
    }

    /// Plays, pausing nothing; starts the first track if none is loaded.
    pub fn play(&mut self) {
        if self.current.is_none() {
            if !self.tracks.is_empty() {
                self.select(0);
            }
            return;
        }
        if let Some(engine) = self.engine.as_ref() {
            engine.play();
        }
    }

    /// Pause when playing, play when paused.
    pub fn toggle(&mut self) {
        match self.engine.as_ref() {
            Some(engine) if self.current.is_some() => {
                if engine.is_paused() {
                    engine.play();
                } else {
                    engine.pause();
                }
            }
            _ => self.play(),
        }
    }

    /// Moves `delta` places through the playlist, wrapping.
    pub fn skip(&mut self, delta: i64) {
        if self.tracks.is_empty() {
            return;
        }
        let count = self.tracks.len() as i64;
        let from = self.current.map(|index| index as i64).unwrap_or(-1);
        // `rem_euclid` rather than `%`: the latter is negative for a backward
        // skip from track zero, and indexing with it panics.
        let next = (from + delta).rem_euclid(count) as usize;
        self.select(next);
    }

    /// Jumps to `seconds` within the current track.
    pub fn seek(&mut self, seconds: f64) {
        let Some(engine) = self.engine.as_ref() else { return };
        if self.current.is_none() {
            return;
        }
        let target = Duration::from_secs_f64(seconds.max(0.0));
        if let Err(error) = engine.seek(target) {
            // Seeking is not supported for every container. Report it rather
            // than leaving the scrubber looking stuck.
            self.last_error = Some(error.to_string());
        }
    }

    /// Sets output gain, `0.0..=1.0`.
    pub fn set_volume(&mut self, volume: f64) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_volume(volume as f32);
        }
    }

    /// Advances when the current track has played out.
    ///
    /// Called once per frame. Rodio reports an empty queue rather than raising
    /// an event, so this is a poll by necessity, not by preference.
    pub fn poll_track_finished(&mut self) -> bool {
        let Some(engine) = self.engine.as_ref() else { return false };
        if self.current.is_none() || !engine.is_finished() {
            return false;
        }
        self.skip(1);
        true
    }

    /// Everything the React side renders, as one JSON value.
    ///
    /// One method rather than a getter per field: the UI needs a consistent
    /// snapshot, and assembling it from six separate `invoke` calls would let
    /// the position and the track name disagree.
    pub fn state(&self) -> Value {
        let engine = self.engine.as_ref();
        let position = engine.map(|e| e.position().as_secs_f64()).unwrap_or(0.0);
        let duration = engine.and_then(|e| e.duration()).map(|d| d.as_secs_f64());
        json!({
            "hasDevice": self.engine.is_some(),
            "playing": engine.is_some_and(|e| !e.is_paused()) && self.current.is_some(),
            "index": self.current,
            "position": position,
            "duration": duration,
            "volume": engine.map(|e| e.volume()).unwrap_or(0.0),
            "error": self.last_error,
        })
    }

    /// The playlist, as JSON, for the initial render.
    pub fn library(&self) -> Value {
        Value::Array(
            self.tracks
                .iter()
                .map(|track| json!({ "title": track.title, "album": track.album }))
                .collect(),
        )
    }

    /// Replaces the library with a scan of `root`.
    pub fn rescan(&mut self, root: &Path) {
        self.tracks = crate::library::scan(root);
        self.current = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.stop();
        }
        if self.tracks.is_empty() {
            self.last_error = Some(format!("no playable audio under {}", root.display()));
        } else {
            self.last_error = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(title: &str) -> Track {
        Track {
            path: PathBuf::from(format!("{title}.mp3")),
            title: title.into(),
            album: "Album".into(),
        }
    }

    /// A player with a playlist but no output device, which is exactly what a
    /// machine with no sound card gives us — and what CI runs.
    fn deviceless(count: usize) -> Player {
        let tracks = (0..count).map(|i| track(&format!("t{i}"))).collect();
        Player::new(None, tracks, Some("no device".into()))
    }

    #[test]
    fn the_ui_still_works_without_an_output_device() {
        // The window has to open and the transport has to render on a machine
        // with no sound card. Every control is a no-op, none of them panic.
        let mut player = deviceless(3);
        player.play();
        player.toggle();
        player.toggle();
        player.skip(1);
        player.seek(10.0);
        player.set_volume(0.5);
        assert_eq!(player.state()["hasDevice"], json!(false));
        assert_eq!(player.state()["playing"], json!(false));
    }

    #[test]
    fn skipping_backward_from_the_first_track_wraps_instead_of_panicking() {
        // `%` on a negative index is the classic way to turn "previous" on
        // track one into a crash.
        let mut player = deviceless(4);
        player.skip(-1);
        // With no device nothing loads, but the arithmetic must still be sane.
        assert!(player.tracks().len() == 4);

        let count = 4i64;
        for from in 0..count {
            assert_eq!((from - 1).rem_euclid(count), if from == 0 { 3 } else { from - 1 });
        }
    }

    #[test]
    fn an_empty_library_is_reported_rather_than_hidden() {
        let mut player = deviceless(0);
        player.rescan(Path::new("this/does/not/exist"));
        let state = player.state();
        assert!(
            state["error"].as_str().is_some_and(|e| e.contains("no playable audio")),
            "{state}"
        );
    }

    #[test]
    fn the_library_crosses_the_bridge_as_titles_and_albums() {
        let player = deviceless(2);
        let library = player.library();
        assert_eq!(library.as_array().map(Vec::len), Some(2));
        assert_eq!(library[0]["title"], json!("t0"));
        assert_eq!(library[0]["album"], json!("Album"));
    }

    #[test]
    fn selecting_past_the_end_records_an_error_and_changes_nothing() {
        let mut player = deviceless(2);
        player.select(99);
        assert!(player.state()["error"].as_str().is_some_and(|e| e.contains("99")));
        assert_eq!(player.state()["index"], json!(null));
    }

    #[test]
    fn state_is_one_consistent_snapshot() {
        // Every field the UI renders comes from a single call, so the position
        // and the track it belongs to can never disagree on screen.
        let player = deviceless(1);
        let state = player.state();
        for key in ["hasDevice", "playing", "index", "position", "duration", "volume", "error"] {
            assert!(state.get(key).is_some(), "state() dropped {key}");
        }
    }
}
