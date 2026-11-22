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

//! Deploying the loader and mods from a build directory into the game folder.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ndml_common::{LOADER_BUILD_DLL, LOADER_MARKER, MOD_PREFIX, Paths, Settings};
use ndml_sdk::config::FileUpdate;

use crate::game::Running;
use crate::modinfo::{self, ModMeta};

/// What occupies `<game>/version.dll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoaderState {
    Missing,
    /// The NDML loader; `up_to_date` if identical to the build being offered.
    Installed {
        up_to_date: bool,
    },
    /// A `version.dll` from another tool; never overwritten.
    Foreign,
    /// A `version.dll` is there but can't be read (in use, no access): it
    /// may not be NDML's, so nothing replaces or removes it.
    Unreadable(String),
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
    let bytes = match std::fs::read(paths.proxy_dll()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoaderState::Missing,
        Err(e) => return LoaderState::Unreadable(e.to_string()),
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

/// What an install did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Installed {
    /// What was copied where (`loader -> …`, `<mod> -> …`).
    pub deployed: Vec<String>,
    /// One entry per mod whose settings file was made, brought up to date
    /// or could not be.
    pub configs: Vec<ConfigNote>,
}

/// What an install did to one mod's settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigNote {
    /// The mod's DLL stem (`ndmod_magnet_range`).
    pub stem: String,
    /// Its package name, as the launcher lists it (`magnet-range`).
    pub name: String,
    pub change: ConfigChange,
}

/// See [`ConfigNote`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigChange {
    Created,
    /// Rewritten; `notice` is the full line (`config refreshed: …`).
    Refreshed {
        notice: String,
        added: usize,
        dropped: usize,
    },
    /// Left as it is: why (malformed, read-only, …).
    Failed(String),
}

impl ConfigNote {
    /// The full line for the player: `ndmod_x: config refreshed: …`.
    #[must_use]
    pub fn line(&self) -> String {
        let what = match &self.change {
            ConfigChange::Created => "config created".to_owned(),
            ConfigChange::Refreshed { notice, .. } => notice.clone(),
            ConfigChange::Failed(e) => format!("unable to update config ({e}); file left as it is"),
        };
        format!("{}: {what}", self.stem)
    }
}

impl Installed {
    /// The settings files in one status line, counts and not the list:
    /// `settings updated: discord-presence (3 added, 1 dropped), magnet-range`,
    /// and whether it needs the player's attention (a file left as it was:
    /// `settings not updated: magnet-range (file is read-only)`, named first).
    /// Past `max` characters, the files whose comments alone changed are
    /// counted instead of named (`, 4 more`), and then all updated files
    /// (`settings updated: 5 files`); failures stay named. `None` when no
    /// file changed.
    #[must_use]
    pub fn settings_summary(&self, max: usize) -> Option<(String, bool)> {
        let failed: Vec<String> = self
            .configs
            .iter()
            .filter_map(|c| match &c.change {
                ConfigChange::Failed(e) => Some(format!("{} ({e})", c.name)),
                _ => None,
            })
            .collect();
        // (text, whether only its comments changed)
        let updated: Vec<(String, bool)> = self
            .configs
            .iter()
            .filter_map(|c| match &c.change {
                ConfigChange::Created => Some((format!("{} (new)", c.name), false)),
                ConfigChange::Refreshed { added, dropped, .. } => {
                    let mut n = Vec::new();
                    if *added > 0 {
                        n.push(format!("{added} added"));
                    }
                    if *dropped > 0 {
                        n.push(format!("{dropped} dropped"));
                    }
                    Some(if n.is_empty() {
                        (c.name.clone(), true)
                    } else {
                        (format!("{} ({})", c.name, n.join(", ")), false)
                    })
                }
                ConfigChange::Failed(_) => None,
            })
            .collect();
        let line = |names: &[String]| {
            let mut parts = Vec::new();
            if !failed.is_empty() {
                parts.push(format!("settings not updated: {}", failed.join(", ")));
            }
            if !names.is_empty() {
                parts.push(format!("settings updated: {}", names.join(", ")));
            }
            parts.join("; ")
        };
        let all: Vec<String> = updated.iter().map(|(t, _)| t.clone()).collect();
        let mut text = line(&all);
        if text.chars().count() > max {
            let mut named: Vec<String> = updated
                .iter()
                .filter(|(_, comments)| !comments)
                .map(|(t, _)| t.clone())
                .collect();
            let more = updated.len() - named.len();
            if more > 0 {
                named.push(format!("{more} more"));
            }
            text = line(&named);
        }
        if text.chars().count() > max && !updated.is_empty() {
            text = line(&[format!("{} files", updated.len())]);
        }
        (!text.is_empty()).then_some((text, !failed.is_empty()))
    }
}

/// Copy the loader (as `version.dll`) and every built mod into the game,
/// and bring each mod's settings file up to date with the settings the mod
/// ships (`ndml_sdk::config::update_file`: written if missing, refreshed
/// if its layout or comments are older; the player's values are kept).
pub fn install(paths: &Paths, build: &Path) -> Result<Installed, String> {
    install_with(paths, build, crate::game::running())
}

/// [`install`] with the game's state given (tests).
pub fn install_with(paths: &Paths, build: &Path, running: Running) -> Result<Installed, String> {
    let mut out = Installed {
        deployed: vec![install_loader_with(paths, build, running)?],
        configs: Vec::new(),
    };
    for m in mod_dlls(build) {
        let dest = paths.mods.join(m.path.file_name().unwrap_or_default());
        copy(&m.path, &dest)?;
        out.deployed
            .push(format!("{} -> {}", m.stem(), dest.display()));
        out.configs.extend(update_config(paths, m.stem(), &dest));
    }
    Ok(out)
}

/// Copy the loader (as `version.dll`) into the game.
fn install_loader_with(paths: &Paths, build: &Path, running: Running) -> Result<String, String> {
    running.ensure_closed()?;
    let loader = built_loader(build).ok_or_else(|| {
        format!(
            "{LOADER_BUILD_DLL} not found in {}; run `cargo build --release`",
            build.display()
        )
    })?;
    match loader_state(paths, build) {
        LoaderState::Foreign => {
            return Err(format!(
                "{} exists and is not NDML; refusing to overwrite",
                paths.proxy_dll().display()
            ));
        }
        LoaderState::Unreadable(e) => {
            return Err(format!(
                "unable to read {} ({e}); nothing changed",
                paths.proxy_dll().display()
            ));
        }
        LoaderState::Missing | LoaderState::Installed { .. } => {}
    }
    paths
        .create_all()
        .map_err(|e| format!("create {}: {e}", paths.root.display()))?;
    if !paths.settings_file().exists() {
        ndml_common::save(&paths.settings_file(), &Settings::default())?;
    }
    copy(&loader.path, &paths.proxy_dll())?;
    Ok(format!("loader -> {}", paths.proxy_dll().display()))
}

/// Make or refresh the settings file of the mod `stem` from the settings
/// `dll` ships. `None` when nothing changed.
fn update_config(paths: &Paths, stem: &str, dll: &Path) -> Option<ConfigNote> {
    let meta = modinfo::read(dll).ok()?;
    let cfg = paths.mod_config(stem);
    let change = match ndml_sdk::config::update_file(&cfg, &meta.default_config, true) {
        Ok(FileUpdate::Created) => ConfigChange::Created,
        Ok(FileUpdate::Refreshed {
            notice,
            added,
            dropped,
        }) => ConfigChange::Refreshed {
            notice,
            added,
            dropped,
        },
        Ok(FileUpdate::Missing | FileUpdate::Unchanged) => return None,
        Err(e) => ConfigChange::Failed(crate::config_edit::by_file_name(&e, &cfg)),
    };
    Some(ConfigNote {
        stem: stem.to_owned(),
        name: meta.package,
        change,
    })
}

/// Where [`copy`] writes before the rename: beside the target, under a name
/// neither Windows (`version.dll`) nor the loader (`ndmod_*.dll`) loads.
fn partial_path(to: &Path) -> PathBuf {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    to.with_file_name(name)
}

/// Copy `from` to `to` whole or not at all: into [`partial_path`] in the
/// same folder, then renamed over `to`. A copy cut short (a full disk, a
/// scanner, a crash) leaves the old file in place and never a truncated
/// DLL the game would import.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("copy {} -> {}: {e}", from.display(), to.display());
    let tmp = partial_path(to);
    let done = std::fs::copy(from, &tmp)
        .map_err(fail)
        .and_then(|_| std::fs::rename(&tmp, to).map_err(fail));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Remove the NDML `version.dll`. Mods, configs and logs under `ndml/` are kept.
pub fn uninstall(paths: &Paths) -> Result<(), String> {
    uninstall_with(paths, crate::game::running())
}

/// [`uninstall`] with the game's state given (tests). One file is removed
/// or nothing is.
pub fn uninstall_with(paths: &Paths, running: Running) -> Result<(), String> {
    running.ensure_closed()?;
    match loader_state(paths, Path::new("")) {
        LoaderState::Missing => Ok(()),
        LoaderState::Foreign => Err("version.dll is not NDML; leaving it alone".into()),
        LoaderState::Unreadable(e) => Err(format!(
            "unable to read version.dll ({e}); leaving it alone"
        )),
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
    crate::game::ensure_closed()?;
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

    /// A `version.dll` that can't be read is not "missing": status says so,
    /// and neither install nor uninstall touches it.
    #[test]
    fn an_unreadable_loader_is_left_alone() {
        let game = temp("unreadable");
        let build = temp("unreadable-build");
        let paths = Paths::new(&game);
        // A folder of that name can't be read as a file.
        std::fs::create_dir_all(paths.proxy_dll()).unwrap();
        assert!(matches!(
            loader_state(&paths, &build),
            LoaderState::Unreadable(_)
        ));
        std::fs::write(build.join(LOADER_BUILD_DLL), b"NDML-LOADER-MARKER").unwrap();
        let e = install_with(&paths, &build, Running::No).unwrap_err();
        assert!(e.starts_with("unable to read "), "{e}");
        let e = uninstall_with(&paths, Running::No).unwrap_err();
        assert!(e.starts_with("unable to read version.dll"), "{e}");
        assert!(paths.proxy_dll().is_dir());
        // Running, or can't tell: nothing either.
        std::fs::remove_dir(paths.proxy_dll()).unwrap();
        for r in [Running::Yes, Running::Unknown] {
            assert!(install_with(&paths, &build, r).is_err());
            assert!(!paths.proxy_dll().exists());
        }
        std::fs::remove_dir_all(game).unwrap();
        std::fs::remove_dir_all(build).unwrap();
    }

    /// Install replaces `version.dll` and the mods whole or not at all: a
    /// copy that can't finish leaves the installed loader as it was (never a
    /// truncated DLL the game would import), and no partial file stays.
    #[test]
    fn a_copy_cut_short_leaves_the_installed_loader() {
        let game = temp("partial");
        let build = temp("partial-build");
        let paths = Paths::new(&game);
        paths.create_all().unwrap();
        let old = b"xxNDML-LOADER-MARKER-v0.1 old".as_slice();
        let new = b"xxNDML-LOADER-MARKER-v0.2 new".as_slice();
        std::fs::write(paths.proxy_dll(), old).unwrap();
        std::fs::write(build.join(LOADER_BUILD_DLL), new).unwrap();
        std::fs::write(build.join("ndmod_m.dll"), b"mod v2").unwrap();
        std::fs::write(paths.mods.join("ndmod_m.dll"), b"mod v1").unwrap();

        // The copy can't complete (a folder where it writes stands in for a
        // full disk or a file held open): an error, the old loader in place.
        let partial = partial_path(&paths.proxy_dll());
        std::fs::create_dir_all(&partial).unwrap();
        let e = install_with(&paths, &build, Running::No).unwrap_err();
        assert!(e.starts_with("copy "), "{e}");
        assert_eq!(std::fs::read(paths.proxy_dll()).unwrap(), old);
        assert_eq!(
            std::fs::read(paths.mods.join("ndmod_m.dll")).unwrap(),
            b"mod v1"
        );
        std::fs::remove_dir(&partial).unwrap();

        // Then it completes: the new loader and mod, nothing left beside them.
        install_with(&paths, &build, Running::No).unwrap();
        assert_eq!(std::fs::read(paths.proxy_dll()).unwrap(), new);
        assert_eq!(
            std::fs::read(paths.mods.join("ndmod_m.dll")).unwrap(),
            b"mod v2"
        );
        for dir in [&game, &paths.mods] {
            let left: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".partial"))
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }
        std::fs::remove_dir_all(game).unwrap();
        std::fs::remove_dir_all(build).unwrap();
    }

    /// Install brings an older settings file up to date and says so; the
    /// player's values stay, the old file is kept as `.bak`.
    #[test]
    fn install_refreshes_settings_files() {
        let game = temp("refresh");
        let build = temp("refresh-build");
        let paths = Paths::new(&game);
        std::fs::write(build.join(LOADER_BUILD_DLL), b"NDML-LOADER-MARKER").unwrap();
        let shipped = "# How far.\nrange = 3.0\n# New.\nfresh = 20\n";
        let record =
            format!("junk\u{1}NDML-MODINFO\u{2}m\u{1f}0.2.0\u{1f}About\u{1f}{shipped}\u{3}junk");
        std::fs::write(build.join("ndmod_m.dll"), record).unwrap();
        paths.create_all().unwrap();
        let cfg = paths.mod_config("ndmod_m");
        std::fs::write(&cfg, "# old words\nrange = 5.5\nretired = true\n").unwrap();
        let done = install_with(&paths, &build, Running::No).unwrap();
        let lines: Vec<String> = done.configs.iter().map(ConfigNote::line).collect();
        assert_eq!(
            lines,
            ["ndmod_m: config refreshed: fresh added (20), retired dropped (was true)"]
        );
        assert_eq!(
            done.settings_summary(200),
            Some(("settings updated: m (1 added, 1 dropped)".into(), false))
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "# How far.\nrange = 5.5\n# New.\nfresh = 20\n"
        );
        assert!(
            std::fs::read_to_string(cfg.with_file_name("ndmod_m.toml.bak"))
                .unwrap()
                .contains("retired = true")
        );
        // Again: nothing to say.
        assert!(
            install_with(&paths, &build, Running::No)
                .unwrap()
                .configs
                .is_empty()
        );
        // A malformed file is reported and left as it is.
        std::fs::write(&cfg, "range = [").unwrap();
        let done = install_with(&paths, &build, Running::No).unwrap();
        assert!(
            done.configs[0].line().contains("unable to update config"),
            "{done:?}"
        );
        let (line, attention) = done.settings_summary(200).unwrap();
        assert!(
            attention && line.starts_with("settings not updated: m ("),
            "{line}"
        );
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "range = [");
        std::fs::remove_dir_all(game).unwrap();
        std::fs::remove_dir_all(build).unwrap();
    }

    /// The status line counts, it does not list: failures first.
    #[test]
    fn settings_summary_counts() {
        let note = |name: &str, change| ConfigNote {
            stem: format!("ndmod_{name}"),
            name: name.into(),
            change,
        };
        let refreshed = |added, dropped| ConfigChange::Refreshed {
            notice: String::new(),
            added,
            dropped,
        };
        let done = Installed {
            deployed: Vec::new(),
            configs: vec![
                note("demo", refreshed(56, 2)),
                note("magnet-range", refreshed(0, 0)),
                note("x", ConfigChange::Created),
                note("y", ConfigChange::Failed("file is read-only".into())),
            ],
        };
        assert_eq!(
            done.settings_summary(200),
            Some((
                "settings not updated: y (file is read-only); settings updated: demo (56 added, 2 dropped), magnet-range, x (new)"
                    .into(),
                true
            ))
        );
        // Too long for the line: files with new comments only are counted.
        assert_eq!(
            done.settings_summary(110).unwrap().0,
            "settings not updated: y (file is read-only); settings updated: demo (56 added, 2 dropped), x (new), 1 more"
        );
        assert_eq!(
            done.settings_summary(60).unwrap().0,
            "settings not updated: y (file is read-only); settings updated: 3 files"
        );
        assert_eq!(Installed::default().settings_summary(200), None);
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
