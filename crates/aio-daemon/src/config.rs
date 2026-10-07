//! `%ProgramData%\aio-ui\config.toml`. Only the daemon writes it; clients
//! change settings over IPC.

use std::fs;
use std::io;
use std::path::Path;

use aio_ipc::{PlayMode, Source};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paused: bool,
    /// Backlight level 0..=100, set at every connect.
    pub brightness: u8,
    /// Screen rotation in degrees (0, 90, 180, 270); unset leaves the
    /// device's own setting (180 by default).
    pub rotation: Option<u16>,
    /// What to show at startup: the selected source, or a random library item.
    pub play_mode: PlayMode,
    /// Id for the next library item (ids are never reused).
    pub next_item_id: u64,
    /// Cache entry of `source`, so startup doesn't need to re-read the file.
    pub cache_key: Option<String>,
    pub source: Option<Source>,
    /// Imported media to pick from; their cached frames are kept.
    pub library: Vec<LibraryEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            paused: false,
            brightness: 100,
            rotation: None,
            play_mode: PlayMode::Selected,
            next_item_id: 1,
            cache_key: None,
            source: None,
            library: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub id: u64,
    /// Cache entry of `source` (frames are imported when the item is added).
    pub cache_key: String,
    pub source: Source,
}

impl Config {
    /// Cache entries that must survive pruning: the current source and the
    /// whole library.
    pub fn keep_keys(&self) -> Vec<String> {
        self.cache_key.iter().cloned().chain(self.library.iter().map(|e| e.cache_key.clone())).collect()
    }

    pub fn item(&self, id: u64) -> Option<&LibraryEntry> {
        self.library.iter().find(|e| e.id == id)
    }

    /// The library item currently shown, if the current source is one.
    pub fn current_item(&self) -> Option<u64> {
        let key = self.cache_key.as_deref()?;
        self.library.iter().find(|e| e.cache_key == key).map(|e| e.id)
    }

    /// Adds an imported source; the same content (cache key) can't be added twice.
    pub fn add_item(&mut self, source: Source, cache_key: String) -> Result<u64, String> {
        if let Some(e) = self.library.iter().find(|e| e.cache_key == cache_key) {
            return Err(format!("already in the library as #{} ({})", e.id, e.source.label()));
        }
        let id = self.next_item_id.max(1);
        self.next_item_id = id + 1;
        self.library.push(LibraryEntry { id, cache_key, source });
        Ok(id)
    }

    pub fn remove_item(&mut self, id: u64) -> Result<LibraryEntry, String> {
        let index = self.library.iter().position(|e| e.id == id).ok_or_else(|| format!("no library item #{id}"))?;
        Ok(self.library.remove(index))
    }

    /// A random playable item for `seed`, avoiding the current one when
    /// there is a choice.
    pub fn pick_random(&self, playable: impl Fn(&LibraryEntry) -> bool, seed: u64) -> Option<&LibraryEntry> {
        let current = self.current_item();
        let candidates: Vec<&LibraryEntry> = self.library.iter().filter(|e| playable(e)).collect();
        let choices: Vec<&LibraryEntry> = if candidates.len() > 1 {
            candidates.iter().copied().filter(|e| Some(e.id) != current).collect()
        } else {
            candidates
        };
        (!choices.is_empty()).then(|| choices[(seed % choices.len() as u64) as usize])
    }
}

impl Config {
    /// Missing file → defaults. Unreadable or invalid file → error.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes via a temp file and rename, so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = format!(
            "# Managed by aio-daemon. Change settings with aio-cli or the UI.\n{}",
            toml::to_string(self)?
        );
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join("config.toml");
        let config = Config {
            paused: true,
            brightness: 40,
            rotation: Some(90),
            play_mode: PlayMode::Random,
            next_item_id: 3,
            cache_key: Some("0123456789abcdef".into()),
            source: Some(Source::Gif { path: r"C:\Users\me\anim.gif".into() }),
            library: vec![
                LibraryEntry { id: 1, cache_key: "0123456789abcdef".into(), source: Source::Gif { path: r"C:\Users\me\anim.gif".into() } },
                LibraryEntry { id: 2, cache_key: "fedcba9876543210".into(), source: Source::Video { path: r"C:\Users\me\clip.mp4".into() } },
            ],
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert!(!path.with_extension("toml.tmp").exists());

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("[source]\ntype = \"gif\""), "{text}");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&tmp.path().join("none.toml")).unwrap(), Config::default());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        fs::write(&path, "[source]\ntype = \"color\"\nrgb = [0, 128, 128]\n").unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.source, Some(Source::Color { rgb: [0, 128, 128] }));
        assert!(!c.paused);
        assert_eq!(c.brightness, 100, "missing brightness defaults to full");
    }

    fn gif(n: u32) -> Source {
        Source::Gif { path: format!(r"C:\m\{n}.gif").into() }
    }

    fn with_items(n: u32) -> Config {
        let mut c = Config::default();
        for i in 0..n {
            c.add_item(gif(i), format!("{i:016x}")).unwrap();
        }
        c
    }

    #[test]
    fn library_add_remove_and_dedupe() {
        let mut c = with_items(2);
        assert_eq!(c.library.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2]);
        let err = c.add_item(gif(9), format!("{:016x}", 1)).unwrap_err();
        assert!(err.contains("already in the library as #2"), "{err}");

        assert_eq!(c.remove_item(1).unwrap().id, 1);
        assert!(c.remove_item(1).is_err());
        // Ids are never reused.
        assert_eq!(c.add_item(gif(5), "5".repeat(16)).unwrap(), 3);
    }

    #[test]
    fn keeps_current_and_library_cache() {
        let mut c = with_items(2);
        c.cache_key = Some("aaaaaaaaaaaaaaaa".into());
        let keep = c.keep_keys();
        assert_eq!(keep.len(), 3);
        assert!(keep.contains(&"aaaaaaaaaaaaaaaa".to_string()));
        assert_eq!(c.current_item(), None);
        c.cache_key = Some(format!("{:016x}", 1));
        assert_eq!(c.current_item(), Some(2));
    }

    #[test]
    fn random_pick_avoids_current_and_unplayable() {
        let mut c = with_items(3);
        c.cache_key = Some(c.library[0].cache_key.clone()); // item 1 is showing
        for seed in 0..20 {
            let picked = c.pick_random(|_| true, seed).unwrap().id;
            assert_ne!(picked, 1, "repeated the current item");
        }
        // Only item 1 playable: picked even though it is current.
        assert_eq!(c.pick_random(|e| e.id == 1, 7).unwrap().id, 1);
        // Item 3 unplayable: never picked.
        assert!((0..20).all(|s| c.pick_random(|e| e.id != 3, s).unwrap().id == 2));
        assert!(Config::default().pick_random(|_| true, 1).is_none());
        // Different seeds reach different items.
        let picks: std::collections::HashSet<u64> = (0..20).map(|s| c.pick_random(|_| true, s).unwrap().id).collect();
        assert_eq!(picks.len(), 2);
    }

    #[test]
    fn invalid_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        fs::write(&path, "[source]\ntype = \"firmware\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
