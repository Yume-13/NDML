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

//! Deploying the loader and mods from a build directory into the game folder.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ndml_common::{LOADER_BUILD_DLL, LOADER_MARKER, MOD_PREFIX, Paths, Settings};

use crate::modinfo::{self, ModMeta};

/// What occupies `<game>/version.dll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderState {
    Missing,
    /// The NDML loader; `up_to_date` if identical to the build being offered.
    Installed {
        up_to_date: bool,
    },
    /// A `version.dll` from another tool; never overwritten.
    Foreign,
}

/// A DLL on disk.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

impl Artifact {
    fn new(path: PathBuf) -> Self {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self { path, modified }
    }

    #[must_use]
    pub fn stem(&self) -> &str {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
    }
}

/// Deployment state of one mod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModState {
    /// Deployed copy matches the build.
    Synced,
    /// Build differs from the deployed copy.
    Outdated,
    /// Built but not deployed yet.
    NotDeployed,
    /// Deployed, no matching build (installed by hand or no longer built).
    External,
}

#[derive(Debug, Clone)]
pub struct ModEntry {
    pub stem: String,
    pub built: Option<Artifact>,
    pub deployed: Option<Artifact>,
    pub enabled: bool,
    pub state: ModState,
    pub meta: Result<ModMeta, String>,
}

/// Directory holding the build artifacts: next to the running `ndml.exe`.
#[must_use]
pub fn build_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}

fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[must_use]
pub fn built_loader(build: &Path) -> Option<Artifact> {
    let p = build.join(LOADER_BUILD_DLL);
    p.is_file().then(|| Artifact::new(p))
}

#[must_use]
pub fn loader_state(paths: &Paths, build: &Path) -> LoaderState {
    let Ok(bytes) = std::fs::read(paths.proxy_dll()) else {
        return LoaderState::Missing;
    };
    if !contains(&bytes, LOADER_MARKER) {
        return LoaderState::Foreign;
    }
    let up_to_date = built_loader(build)
        .and_then(|a| std::fs::read(a.path).ok())
        .is_some_and(|b| b == bytes);
    LoaderState::Installed { up_to_date }
}

fn mod_dlls(dir: &Path) -> Vec<Artifact> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<Artifact> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll"))
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with(MOD_PREFIX))
        })
        .map(Artifact::new)
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// Merge built and deployed mods into one list (alphabetical = load order).
#[must_use]
pub fn scan_mods(paths: &Paths, build_dir: &Path, settings: &Settings) -> Vec<ModEntry> {
    let built = mod_dlls(build_dir);
    let deployed = mod_dlls(&paths.mods);
    let mut stems: Vec<String> = built
        .iter()
        .chain(&deployed)
        .map(|a| a.stem().to_owned())
        .collect();
    stems.sort();
    stems.dedup();
    stems
        .into_iter()
        .map(|stem| {
            let build_copy = built.iter().find(|a| a.stem() == stem).cloned();
            let game_copy = deployed.iter().find(|a| a.stem() == stem).cloned();
            let state = match (&build_copy, &game_copy) {
                (Some(b), Some(g)) if same_contents(&b.path, &g.path) => ModState::Synced,
                (Some(_), Some(_)) => ModState::Outdated,
                (Some(_), None) => ModState::NotDeployed,
                (None, _) => ModState::External,
            };
            // Metadata comes from the build copy when present, since that is
            // what the next install deploys.
            let source = build_copy
                .as_ref()
                .or(game_copy.as_ref())
                .map(|a| a.path.clone());
            let meta = source.map_or_else(|| Err("no DLL".to_owned()), |p| modinfo::read(&p));
            let enabled = !settings
                .disabled_mods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&stem));
            ModEntry {
                stem,
                built: build_copy,
                deployed: game_copy,
                enabled,
                state,
                meta,
            }
        })
        .collect()
}

/// Copy the loader (as `version.dll`) and every built mod into the game.
/// Writes default configs for mods that don't have one yet.
pub fn install(paths: &Paths, build: &Path) -> Result<Vec<String>, String> {
    if crate::game::is_running() {
        return Err("Nova Drift is running; close it first".into());
    }
    let loader = built_loader(build).ok_or_else(|| {
        format!(
            "{LOADER_BUILD_DLL} not found in {}; run `cargo build --release`",
            build.display()
        )
    })?;
    if loader_state(paths, build) == LoaderState::Foreign {
        return Err(format!(
            "{} exists and is not NDML; refusing to overwrite",
            paths.proxy_dll().display()
        ));
    }
    paths
        .create_all()
        .map_err(|e| format!("create {}: {e}", paths.root.display()))?;
    if !paths.settings_file().exists() {
        ndml_common::save(&paths.settings_file(), &Settings::default())?;
    }
    let mut done = Vec::new();
    copy(&loader.path, &paths.proxy_dll())?;
    done.push(format!("loader -> {}", paths.proxy_dll().display()));
    for m in mod_dlls(build) {
        let dest = paths.mods.join(m.path.file_name().unwrap_or_default());
        copy(&m.path, &dest)?;
        done.push(format!("{} -> {}", m.stem(), dest.display()));
        write_default_config(paths, m.stem(), &dest);
    }
    Ok(done)
}

fn write_default_config(paths: &Paths, stem: &str, dll: &Path) {
    let cfg = paths.mod_config(stem);
    if cfg.exists() {
        return;
    }
    if let Ok(meta) = modinfo::read(dll) {
        if !meta.default_config.is_empty() {
            let _ = std::fs::write(cfg, meta.default_config);
        }
    }
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))
}

/// Remove the NDML `version.dll`. Mods, configs and logs under `ndml/` are kept.
pub fn uninstall(paths: &Paths) -> Result<(), String> {
    if crate::game::is_running() {
        return Err("Nova Drift is running; close it first".into());
    }
    match loader_state(paths, Path::new("")) {
        LoaderState::Missing => Ok(()),
        LoaderState::Foreign => Err("version.dll is not NDML; leaving it alone".into()),
        LoaderState::Installed { .. } => {
            std::fs::remove_file(paths.proxy_dll()).map_err(|e| format!("remove version.dll: {e}"))
        }
    }
}

/// Enable/disable a mod via `disabled_mods` in `ndml.toml`.
pub fn set_enabled(
    paths: &Paths,
    settings: &mut Settings,
    stem: &str,
    enabled: bool,
) -> Result<(), String> {
    settings
        .disabled_mods
        .retain(|m| !m.eq_ignore_ascii_case(stem));
    if !enabled {
        settings.disabled_mods.push(stem.to_owned());
    }
    ndml_common::save(&paths.settings_file(), settings)
}

/// Remove a deployed mod DLL (its config is kept).
pub fn remove_mod(entry: &ModEntry) -> Result<(), String> {
    if crate::game::is_running() {
        return Err("Nova Drift is running; close it first".into());
    }
    let d = entry.deployed.as_ref().ok_or("mod is not deployed")?;
    std::fs::remove_file(&d.path).map_err(|e| format!("remove {}: {e}", d.path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        // Scratch space under `target/test-tmp`.
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ndml-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_loader_states() {
        let game = temp("game");
        let build = temp("build");
        let paths = Paths::new(&game);
        assert_eq!(loader_state(&paths, &build), LoaderState::Missing);
        std::fs::write(paths.proxy_dll(), b"some other dll").unwrap();
        assert_eq!(loader_state(&paths, &build), LoaderState::Foreign);
        std::fs::write(paths.proxy_dll(), b"xxNDML-LOADER-MARKER-v0.1yy").unwrap();
        assert_eq!(
            loader_state(&paths, &build),
            LoaderState::Installed { up_to_date: false }
        );
        std::fs::write(build.join(LOADER_BUILD_DLL), b"xxNDML-LOADER-MARKER-v0.1yy").unwrap();
        assert_eq!(
            loader_state(&paths, &build),
            LoaderState::Installed { up_to_date: true }
        );
        std::fs::remove_dir_all(game).unwrap();
        std::fs::remove_dir_all(build).unwrap();
    }

    #[test]
    fn enable_disable_roundtrip() {
        let game = temp("toggle");
        let paths = Paths::new(&game);
        paths.create_all().unwrap();
        let mut s = Settings::default();
        set_enabled(&paths, &mut s, "ndmod_x", false).unwrap();
        assert_eq!(s.disabled_mods, vec!["ndmod_x".to_owned()]);
        set_enabled(&paths, &mut s, "NDMOD_X", true).unwrap();
        assert!(s.disabled_mods.is_empty());
        std::fs::remove_dir_all(game).unwrap();
    }
}
