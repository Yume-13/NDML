// MIT License
//
// Copyright © 2026 Yume-13
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
//!     logs/ndml.log        <- this launch's log; the last nine launches'
//!                             beside it as ndml-<start, local>.log (log_keep)
//! ```
//! All state is local to the game folder; nothing is written to `AppData`.

#![forbid(unsafe_code)]

pub mod log_keep;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub log_level: String,
    /// Open a console window mirroring the log.
    pub console: bool,
    /// Mod DLL stems to skip (e.g. `["ndmod_magnet_range"]`).
    pub disabled_mods: Vec<String>,
    /// Measure the loader's `ExecuteIt` detour (event guard, event observers)
    /// and log its cost per frame every 10 s.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub event_stats: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            console: false,
            disabled_mods: Vec::new(),
            event_stats: false,
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

/// The top-level keys [`Settings`] reads: the field names serde asks for.
fn settings_keys() -> &'static [&'static str] {
    use serde::de::{self, Visitor};
    use std::cell::Cell;

    /// Records the field list serde passes to `deserialize_struct`, then
    /// stops with an error.
    struct Fields<'a>(&'a Cell<&'static [&'static str]>);

    #[derive(Debug)]
    struct Stop;
    impl std::fmt::Display for Stop {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("field list taken")
        }
    }
    impl std::error::Error for Stop {}
    impl de::Error for Stop {
        fn custom<M: std::fmt::Display>(_: M) -> Self {
            Stop
        }
    }
    impl<'de> de::Deserializer<'de> for Fields<'_> {
        type Error = Stop;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop)
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Stop> {
            self.0.set(fields);
            Err(Stop)
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }

    let seen = Cell::new(&[][..]);
    let _ = Settings::deserialize(Fields(&seen));
    seen.get()
}

/// `ndml.toml`'s text as settings, key by key: a key the settings do not
/// have is skipped and a value they refuse leaves that key at its default,
/// each named in one line (`ndml.toml: x unknown; skipped`), so one stray
/// key never resets the rest. `Err` for text that is not TOML.
pub fn parse(text: &str) -> Result<(Settings, Vec<String>), String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        e.to_string().lines().next().unwrap_or_default().to_owned()
    })?;
    let known = settings_keys();
    let mut good = toml::Table::new();
    let mut problems = Vec::new();
    for (key, value) in table {
        if !known.contains(&key.as_str()) {
            problems.push(format!("ndml.toml: {key} unknown; skipped"));
            continue;
        }
        let mut trial = good.clone();
        trial.insert(key.clone(), value.clone());
        if toml::Value::Table(trial.clone())
            .try_into::<Settings>()
            .is_ok()
        {
            good = trial;
        } else {
            problems.push(format!(
                "ndml.toml: {key} = {value} is not valid; using the default"
            ));
        }
    }
    let settings = toml::Value::Table(good).try_into().unwrap_or_default();
    Ok((settings, problems))
}

/// Load settings, writing defaults only when there is no file. A file that
/// can't be read (locked, not UTF-8) or parsed gives the defaults for this
/// launch and is left as it is. A key the settings don't have, or a value
/// they refuse, costs only that key ([`parse`]). Returns the lines to log
/// once logging is up.
#[must_use]
pub fn load(path: &Path) -> (Settings, Vec<String>) {
    let failed = |e: &dyn std::fmt::Display| {
        vec![format!(
            "unable to read ndml.toml ({}: {e}); using defaults",
            path.display()
        )]
    };
    match std::fs::read_to_string(path) {
        Ok(text) => {
            return match parse(&text) {
                Ok(read) => read,
                Err(e) => (Settings::default(), failed(&e)),
            };
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return (Settings::default(), failed(&e));
        }
        Err(_) => {}
    }
    let s = Settings::default();
    let err = toml::to_string_pretty(&s)
        .map_err(|e| e.to_string())
        .and_then(|t| write_atomic(path, &t).map_err(|e| e.to_string()))
        .err();
    (s, err.map_or_default(|e| failed(&e)))
}

/// Replace `path` with `text` through `<name>.tmp` beside it and a rename,
/// so a reader never sees half a file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Read settings without creating the file, leaving the game folder untouched.
/// Missing → defaults; see [`read_noting`].
#[must_use]
pub fn read(path: &Path) -> Settings {
    read_noting(path).0
}

/// [`read`], with what the reader should be told: each key skipped or left
/// at its default ([`parse`]), or a file that can't be read or parsed
/// (defaults shown).
#[must_use]
pub fn read_noting(path: &Path) -> (Settings, Vec<String>) {
    match read_checked(path) {
        Ok(read) => read,
        Err(e) => (
            Settings::default(),
            vec![format!("unable to read {e}; showing the defaults")],
        ),
    }
}

/// Read settings for a change: missing → defaults; unreadable or not TOML
/// → `Err` (so a save never replaces a file it could not read). A key the
/// settings don't have is skipped (the save then leaves it out).
pub fn read_checked(path: &Path) -> Result<(Settings, Vec<String>), String> {
    match std::fs::read_to_string(path) {
        Ok(t) => parse(&t).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Settings::default(), Vec::new())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write settings back (used by the manager).
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let text = toml::to_string_pretty(settings).map_err(|e| e.to_string())?;
    write_atomic(path, &text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let text = toml::to_string_pretty(&Settings::default()).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.level_filter(), log::LevelFilter::Info);
    }

    /// Scratch folder under `target/test-tmp`.
    fn scratch(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ndml-common-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A settings file that can't be read is not replaced: the defaults
    /// for this launch, the error to log, the file as it was. Only a
    /// missing file is written.
    #[test]
    fn a_settings_file_that_cannot_be_read_is_not_replaced() {
        let dir = scratch("load");
        // Not UTF-8 (as a read that fails would be): refused, bytes kept.
        let path = dir.join("ndml.toml");
        let bytes = b"disabled_mods = [\"ndmod_x\"]\n\xff\n";
        std::fs::write(&path, bytes).unwrap();
        let (s, e) = load(&path);
        assert!(s.disabled_mods.is_empty() && e.len() == 1 && e[0].contains("ndml.toml"));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // A folder in the file's place: an error, nothing written.
        let locked = dir.join("locked.toml");
        std::fs::create_dir_all(&locked).unwrap();
        assert_eq!(load(&locked).1.len(), 1);
        assert!(locked.is_dir());
        // No file: the defaults are written.
        let fresh = dir.join("fresh.toml");
        assert_eq!(load(&fresh).1.len(), 0);
        assert!(read_checked(&fresh).is_ok_and(|(s, _)| s.disabled_mods.is_empty()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// An unknown key or a refused value costs only that key, named once:
    /// `disabled_mods` and the rest are kept, and the file is left as it is.
    #[test]
    fn an_unknown_key_keeps_disabled_mods() {
        let text = "log_levle = \"debug\"\nconsole = \"yes\"\ndisabled_mods = [\"ndmod_x\"]\nlog_level = \"warn\"\n";
        let (s, problems) = parse(text).unwrap();
        assert_eq!(s.disabled_mods, ["ndmod_x"]);
        assert_eq!(s.log_level, "warn");
        assert!(!s.console, "a refused value: its default");
        assert_eq!(
            problems,
            [
                "ndml.toml: console = \"yes\" is not valid; using the default",
                "ndml.toml: log_levle unknown; skipped",
            ]
        );
        assert!(parse("disabled_mods = [").is_err());
        assert!(
            settings_keys().contains(&"disabled_mods") && settings_keys().contains(&"event_stats")
        );

        let dir = scratch("unknown");
        let path = dir.join("ndml.toml");
        std::fs::write(&path, text).unwrap();
        let (s, lines) = load(&path);
        assert_eq!((s.disabled_mods.len(), lines.len()), (1, 2));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let (s, notes) = read_noting(&path);
        assert_eq!((s.disabled_mods.len(), notes.len()), (1, 2));
        std::fs::write(&path, "disabled_mods = [").unwrap();
        let (s, notes) = read_noting(&path);
        assert!(s.disabled_mods.is_empty() && notes[0].starts_with("unable to read "));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
