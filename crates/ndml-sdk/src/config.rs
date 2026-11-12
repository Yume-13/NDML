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

//! TOML config files for mods.
//!
//! A mod ships its defaults as a TOML file (`mods/<mod>/config.toml`, embedded
//! with `export_mod!(init, defaults = include_str!("../config.toml"))`). That
//! text is the single source of truth: it is written verbatim (comments and all)
//! when no config file exists yet, and an existing file is overlaid on it key by
//! key, so missing keys fall back to the shipped values.

#![forbid(unsafe_code)]

use std::path::Path;

use serde::de::DeserializeOwned;

/// Parse the shipped defaults alone. Use this in a mod's unit test to prove its
/// `config.toml` matches its `Config` type.
pub fn parse_defaults<T: DeserializeOwned>(defaults: &str) -> Result<T, String> {
    toml::from_str(defaults).map_err(|e| format!("built-in defaults are invalid: {e}"))
}

/// Load `path` over `defaults`, creating the file from `defaults` if missing.
///
/// A malformed or mistyped config file is logged and ignored, and left untouched
/// on disk. Only invalid *built-in* defaults are an error (a defect in the mod).
pub fn load<T: DeserializeOwned>(path: &Path, defaults: &str) -> Result<T, String> {
    let base: toml::Table = defaults
        .parse()
        .map_err(|e| format!("built-in defaults are invalid: {e}"))?;
    let mut merged = base.clone();
    match std::fs::read_to_string(path) {
        Ok(text) => match text.parse::<toml::Table>() {
            Ok(user) => merged.extend(user),
            Err(e) => log::error!("{}: {e}; using defaults", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !defaults.is_empty() {
                match std::fs::write(path, defaults) {
                    Ok(()) => log::info!("wrote default config {}", path.display()),
                    Err(e) => log::warn!("could not write {}: {e}", path.display()),
                }
            }
        }
        Err(e) => log::error!("{}: {e}; using defaults", path.display()),
    }
    toml::Value::Table(merged).try_into().or_else(|e| {
        log::error!("{}: {e}; using defaults", path.display());
        toml::Value::Table(base)
            .try_into()
            .map_err(|e| format!("built-in defaults are invalid: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, PartialEq, Debug)]
    #[serde(deny_unknown_fields)]
    struct Cfg {
        a: f64,
        b: bool,
    }

    const DEFAULTS: &str = "# comment kept\na = 2.0\nb = true\n";

    #[test]
    fn creates_overlays_and_recovers() {
        // Scratch directory under the workspace `target/`.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ndml-sdk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.toml");
        let _ = std::fs::remove_file(&path);

        let first: Cfg = load(&path, DEFAULTS).unwrap();
        assert_eq!(first, Cfg { a: 2.0, b: true });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULTS);

        std::fs::write(&path, "a = 5.0\n").unwrap(); // partial file: b from defaults
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 5.0, b: true }
        );

        std::fs::write(&path, "a = \"oops\"\n").unwrap(); // wrong type → defaults
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 2.0, b: true }
        );

        std::fs::write(&path, "a = [nonsense").unwrap(); // syntax error → defaults
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 2.0, b: true }
        );

        assert!(parse_defaults::<Cfg>("a = 1").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
