// MIT License
//
// Copyright (c) 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! # ndml-common
//!
//! The on-disk contract between the loader (`version.dll`, inside the game) and
//! the `ndml` manager: where things live in the game folder, the loader's
//! settings file, and the marker that identifies the loader's proxy DLL.
//!
//! ```text
//! <Nova Drift>/
//!   version.dll            <- the loader (renamed ndml_loader.dll)
//!   ndml/
//!     ndml.toml            <- Settings
//!     mods/ndmod_*.dll     <- mods, loaded alphabetically
//!     config/<mod>.toml    <- per-mod config (written by mods / the manager)
//!     logs/ndml.log        <- truncated each launch
//! ```
//! All state is local to the game folder; nothing is written to `AppData`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub log_level: String,
    /// Open a console window mirroring the log.
    pub console: bool,
    /// Mod DLL stems to skip (e.g. `["ndmod_magnet_range"]`).
    pub disabled_mods: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            console: false,
            disabled_mods: Vec::new(),
        }
    }
}

impl Settings {
    #[must_use]
    pub fn level_filter(&self) -> log::LevelFilter {
        self.log_level.parse().unwrap_or(log::LevelFilter::Info)
    }
}

/// Name the loader is deployed under (a system DLL the game imports; see `docs/how-it-works.md`).
pub const PROXY_DLL: &str = "version.dll";
/// File name cargo produces for the loader.
pub const LOADER_BUILD_DLL: &str = "ndml_loader.dll";
/// Mod DLLs are named `ndmod_<name>.dll`.
pub const MOD_PREFIX: &str = "ndmod_";
/// Byte string embedded in the loader so tools can recognise the NDML `version.dll`.
pub const LOADER_MARKER: &[u8] = b"NDML-LOADER-MARKER";
/// The game executable.
pub const GAME_EXE: &str = "NovaDrift.exe";

/// Directory layout under the game folder.
#[derive(Debug, Clone)]
pub struct Paths {
    pub game: PathBuf,
    pub root: PathBuf,
    pub mods: PathBuf,
    pub config: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    #[must_use]
    pub fn new(game_dir: &Path) -> Self {
        let root = game_dir.join("ndml");
        Self {
            game: game_dir.to_path_buf(),
            mods: root.join("mods"),
            config: root.join("config"),
            logs: root.join("logs"),
            root,
        }
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("ndml.toml")
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.logs.join("ndml.log")
    }

    #[must_use]
    pub fn proxy_dll(&self) -> PathBuf {
        self.game.join(PROXY_DLL)
    }

    /// Config file for the mod whose DLL stem is `mod_stem`.
    #[must_use]
    pub fn mod_config(&self, mod_stem: &str) -> PathBuf {
        self.config.join(format!("{mod_stem}.toml"))
    }

    pub fn create_all(&self) -> std::io::Result<()> {
        for d in [&self.mods, &self.config, &self.logs] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

/// Load settings, writing defaults if absent. Errors are returned alongside
/// defaults so they can be logged once logging is up.
#[must_use]
pub fn load(path: &Path) -> (Settings, Option<String>) {
    if let Ok(text) = std::fs::read_to_string(path) {
        return match toml::from_str(&text) {
            Ok(s) => (s, None),
            Err(e) => (
                Settings::default(),
                Some(format!("{}: {e}", path.display())),
            ),
        };
    }
    let s = Settings::default();
    let err = toml::to_string_pretty(&s)
        .map_err(|e| e.to_string())
        .and_then(|t| std::fs::write(path, t).map_err(|e| e.to_string()))
        .err();
    (s, err)
}

/// Read settings without creating the file, leaving the game folder untouched.
/// Missing or malformed → defaults.
#[must_use]
pub fn read(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// Write settings back (used by the manager).
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let text = toml::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let text = toml::to_string_pretty(&Settings::default()).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.level_filter(), log::LevelFilter::Info);
        assert!(toml::from_str::<Settings>("bogus = 1").is_err());
    }
}
