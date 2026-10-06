//! `%ProgramData%\aio-ui\config.toml`. Only the daemon writes it; clients
//! change settings over IPC.

use std::fs;
use std::io;
use std::path::Path;

use aio_ipc::Source;
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
    /// Cache entry of `source`, so startup doesn't need to re-read the file.
    pub cache_key: Option<String>,
    pub source: Option<Source>,
}

impl Default for Config {
    fn default() -> Self {
        Self { paused: false, brightness: 100, rotation: None, cache_key: None, source: None }
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
            cache_key: Some("0123456789abcdef".into()),
            source: Some(Source::Gif { path: r"C:\Users\me\anim.gif".into() }),
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

    #[test]
    fn invalid_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.toml");
        fs::write(&path, "[source]\ntype = \"firmware\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
