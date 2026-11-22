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

//! TUI state and input handling (no drawing here; see `ui.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ndml_common::{Paths, Settings};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::fx::Starfield;
use super::picker::Picker;
use crate::config_edit::{Adjusted, ConfigDoc};
use crate::deploy::{self, LoaderState, ModEntry, ModState};
use crate::game::{self, Running};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Mods,
    Log,
    Loader,
    Help,
}

impl Tab {
    pub const ALL: [Self; 4] = [Self::Mods, Self::Log, Self::Loader, Self::Help];

    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Mods => "MODS",
            Self::Log => "LOG",
            Self::Loader => "LOADER",
            Self::Help => "HELP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Config,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Uninstall,
    RemoveMod,
    ResetConfig,
}

impl Confirm {
    #[must_use]
    pub fn prompt(self) -> &'static str {
        match self {
            Self::Uninstall => "Remove the loader (version.dll) from the game folder?",
            Self::RemoveMod => "Remove this mod's DLL from the game? (config is kept)",
            Self::ResetConfig => "Reset this mod's config to its defaults?",
        }
    }
}

pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

/// Show the safety notice (offline operation, untrusted mods) at startup.
pub const SHOW_SAFETY_NOTICE: bool = false;

/// Text of the startup safety notice, one entry per line.
pub const SAFETY_NOTICE: [&str; 5] = [
    "NDML works fully offline: it never needs or uses internet access.",
    "Its complete source code should be included with this download.",
    "",
    "Mods are native programs with the same access as the game itself.",
    "Only install mods from sources you trust.",
];
const LOG_TAIL: usize = 2000;
/// The most of the log's end read for [`LOG_TAIL`] lines: 2000 lines of
/// more than 500 bytes each is past any real log.
const LOG_TAIL_BYTES: u64 = 1 << 20;

/// The last [`LOG_TAIL`] lines of the log at `path`, read from its last
/// [`LOG_TAIL_BYTES`] (the partial first line of a cut is left out).
/// `None` when it can't be read.
fn log_tail(path: &Path) -> Option<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(LOG_TAIL);
    Some(lines[start..].iter().map(|s| (*s).to_owned()).collect())
}

#[derive(Debug)]
pub struct App {
    pub paths: Option<Paths>,
    pub build: PathBuf,
    pub tab: Tab,
    pub focus: Focus,
    pub settings: Settings,
    pub mods: Vec<ModEntry>,
    pub mod_sel: usize,
    pub config: Option<Result<ConfigDoc, String>>,
    pub cfg_sel: usize,
    pub editing: Option<String>,
    pub loader: LoaderState,
    pub running: Running,
    pub log: Vec<String>,
    /// The log file's modified time and length when `log` was read.
    log_stamp: Option<(Option<std::time::SystemTime>, u64)>,
    /// Lines scrolled up from the bottom of the log.
    pub log_scroll: usize,
    pub loader_sel: usize,
    pub status: Option<(String, Tone, Instant)>,
    pub confirm: Option<Confirm>,
    /// The checklist of a list setting, while it is open.
    pub picker: Option<Picker>,
    /// The reading panel (`?`) is open, scrolled this many lines.
    pub reading: Option<usize>,
    /// The log tab shows warnings and errors only.
    pub log_filter: bool,
    /// The startup safety notice is on screen (any key dismisses it).
    pub notice: bool,
    pub stars: Starfield,
    pub tick: u64,
    pub quit: bool,
    last_poll: Instant,
    watcher: game::RunningWatcher,
}

impl App {
    #[must_use]
    pub fn new(game_dir: Option<&Path>) -> Self {
        let paths = game_dir.map(Paths::new);
        let mut app = Self {
            paths,
            build: deploy::build_dir(),
            tab: Tab::Mods,
            focus: Focus::List,
            settings: Settings::default(),
            mods: Vec::new(),
            mod_sel: 0,
            config: None,
            cfg_sel: 0,
            editing: None,
            loader: LoaderState::Missing,
            running: Running::No,
            log: Vec::new(),
            log_stamp: None,
            log_scroll: 0,
            loader_sel: 0,
            status: None,
            confirm: None,
            picker: None,
            reading: None,
            log_filter: false,
            notice: SHOW_SAFETY_NOTICE,
            stars: Starfield::new(0x9E37_79B9_7F4A_7C15),
            tick: 0,
            quit: false,
            last_poll: Instant::now(),
            watcher: game::RunningWatcher::spawn(),
        };
        app.refresh();
        if app.paths.is_none() {
            app.say(
                "Nova Drift not found — set NOVA_DRIFT_DIR or pass --game-dir",
                Tone::Bad,
            );
        } else {
            app.say("Systems online. Welcome back, pilot.", Tone::Good);
        }
        app
    }

    pub fn say(&mut self, msg: impl Into<String>, tone: Tone) {
        self.status = Some((msg.into(), tone, Instant::now()));
    }

    /// A change that worked; while the game runs (or that can't be told)
    /// it takes effect on the next launch, and the line says so.
    fn say_saved(&mut self, msg: &str) {
        if self.running == Running::No {
            self.say(msg, Tone::Good);
        } else {
            self.say(format!("{msg} (applies on next launch)"), Tone::Warn);
        }
    }

    fn report<T>(&mut self, result: Result<T, String>, ok_msg: &str) -> bool {
        match result {
            Ok(_) => {
                self.say_saved(ok_msg);
                true
            }
            Err(e) => {
                self.say(e, Tone::Bad);
                false
            }
        }
    }

    /// Full rescan: settings, mods (reads DLL metadata), loader, config, log.
    pub fn refresh(&mut self) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        let (settings, problems) = ndml_common::read_noting(&paths.settings_file());
        self.settings = settings;
        self.mods = deploy::scan_mods(&paths, &self.build, &self.settings);
        self.mod_sel = self.mod_sel.min(self.mods.len().saturating_sub(1));
        self.loader = deploy::loader_state(&paths, &self.build);
        self.load_config();
        self.poll();
        // What the loader will also warn about: shown on the status line,
        // the first one and how many more.
        if let Some(first) = problems.first() {
            let more = match problems.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            self.say(format!("{first}{more}"), Tone::Warn);
        }
    }

    /// Cheap periodic refresh (game process, log tail).
    pub fn poll(&mut self) {
        self.last_poll = Instant::now();
        self.running = self.watcher.get();
        // The game may save a choice into the open file: show it.
        let failed = match self.config.as_mut() {
            Some(Ok(doc)) => doc
                .reload_if_changed()
                .err()
                .map(|e| crate::config_edit::by_file_name(&e, &doc.path)),
            _ => None,
        };
        if let Some(e) = failed {
            self.say(e, Tone::Bad);
        }
        if let Some(paths) = &self.paths {
            // Only when the log changed, and only its end.
            let path = paths.log_file();
            let stamp = std::fs::metadata(&path)
                .ok()
                .map(|m| (m.modified().ok(), m.len()));
            if stamp.is_some()
                && stamp != self.log_stamp
                && let Some(lines) = log_tail(&path)
            {
                self.log = lines;
                self.log_stamp = stamp;
            }
        }
    }

    pub fn on_tick(&mut self, width: u16, height: u16) {
        self.tick = self.tick.wrapping_add(1);
        self.stars.step(width, height);
        if self.last_poll.elapsed() > Duration::from_secs(1) {
            self.poll();
        }
    }

    #[must_use]
    pub fn selected_mod(&self) -> Option<&ModEntry> {
        self.mods.get(self.mod_sel)
    }

    /// Select the first mod whose name contains `filter` and focus its config
    /// (snapshot aid).
    pub fn focus_mod_config(&mut self, filter: &str) {
        if let Some(i) = self.mods.iter().position(|m| m.stem.contains(filter)) {
            self.mod_sel = i;
            self.load_config();
            self.focus = Focus::Config;
        }
    }

    fn load_config(&mut self) {
        self.config = match (&self.paths, self.selected_mod()) {
            (Some(paths), Some(m)) => {
                let default = m.meta.as_ref().map_or_default(|i| i.default_config.clone());
                Some(ConfigDoc::load(&paths.mod_config(&m.stem), &default))
            }
            _ => None,
        };
        let n = self.config_len();
        self.cfg_sel = self.cfg_sel.min(n.saturating_sub(1));
    }

    fn config_len(&self) -> usize {
        match &self.config {
            Some(Ok(doc)) => doc.fields().len(),
            _ => 0,
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if self.notice {
            self.notice = false;
            return;
        }
        if let Some(c) = self.confirm {
            match k.code {
                KeyCode::Char('y' | 'Y') => {
                    self.confirm = None;
                    self.run_confirmed(c);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm = None;
                    self.say("Cancelled.", Tone::Info);
                }
                _ => {}
            }
            return;
        }
        if self.picker.is_some() {
            self.on_picker_key(k);
            return;
        }
        if let Some(at) = self.reading {
            self.on_reading_key(k, at);
            return;
        }
        if self.editing.is_some() {
            self.on_edit_key(k);
            return;
        }
        if self.tab == Tab::Mods && k.code == KeyCode::Char('?') {
            self.reading = Some(0);
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char(d @ '1'..='4') => self.tab = Tab::ALL[(d as u8 - b'1') as usize],
            KeyCode::F(n @ 1..=4) => self.tab = Tab::ALL[usize::from(n - 1)],
            KeyCode::Char('i') => self.install(),
            KeyCode::Char('u') if self.loader != LoaderState::Missing => {
                self.confirm = Some(Confirm::Uninstall);
            }
            KeyCode::Char('g') => self.launch(),
            KeyCode::Char('r') => {
                self.refresh();
                self.say("Rescanned.", Tone::Info);
            }
            _ => match self.tab {
                Tab::Mods => self.on_mods_key(k),
                Tab::Log => self.on_log_key(k),
                Tab::Loader => self.on_loader_key(k),
                Tab::Help => {}
            },
        }
    }

    fn on_mods_key(&mut self, k: KeyEvent) {
        let big = k.modifiers.contains(KeyModifiers::SHIFT);
        match (self.focus, k.code) {
            (_, KeyCode::Tab | KeyCode::BackTab) => {
                self.focus = if self.focus == Focus::List {
                    Focus::Config
                } else {
                    Focus::List
                };
            }
            (Focus::List, KeyCode::Up | KeyCode::Char('k')) => self.move_mod(-1),
            (Focus::List, KeyCode::Down | KeyCode::Char('j')) => self.move_mod(1),
            (Focus::List, KeyCode::Right | KeyCode::Char('l')) => self.focus = Focus::Config,
            (Focus::List, KeyCode::Char(' ') | KeyCode::Enter) => self.toggle_mod(),
            (Focus::List, KeyCode::Char('x')) => {
                if self.selected_mod().is_some_and(|m| m.deployed.is_some()) {
                    self.confirm = Some(Confirm::RemoveMod);
                }
            }
            (Focus::Config, KeyCode::Esc) => self.focus = Focus::List,
            (Focus::Config, KeyCode::Up | KeyCode::Char('k')) => {
                self.cfg_sel = self.cfg_sel.saturating_sub(1);
            }
            (Focus::Config, KeyCode::Down | KeyCode::Char('j')) => {
                self.cfg_sel = (self.cfg_sel + 1).min(self.config_len().saturating_sub(1));
            }
            (Focus::Config, KeyCode::Left | KeyCode::Char('h')) => {
                self.adjust(if big { -10 } else { -1 });
            }
            (Focus::Config, KeyCode::Right | KeyCode::Char('l')) => {
                self.adjust(if big { 10 } else { 1 });
            }
            (Focus::Config, KeyCode::Char(' ')) => self.adjust(1),
            (Focus::Config, KeyCode::Enter) => {
                if let Some(Ok(doc)) = &self.config
                    && let Some(f) = doc.fields().get(self.cfg_sel)
                {
                    if f.kind == "multi" {
                        self.picker = Some(Picker::open(f, self.cfg_sel));
                    } else {
                        self.editing = Some(f.value.trim_matches('"').to_owned());
                    }
                }
            }
            (Focus::Config, KeyCode::Char('d')) if self.config_len() > 0 => {
                self.confirm = Some(Confirm::ResetConfig);
            }
            _ => {}
        }
    }

    fn on_edit_key(&mut self, k: KeyEvent) {
        let Some(buf) = self.editing.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => self.editing = None,
            KeyCode::Backspace => {
                buf.pop();
            }
            // Long enough for any path; the row scrolls to keep the cursor.
            KeyCode::Char(c) if buf.chars().count() < 1024 => buf.push(c),
            KeyCode::Enter => {
                let input = self.editing.take().unwrap_or_default();
                if let Some(Ok(doc)) = self.config.as_mut() {
                    let r = doc.set_from_str(self.cfg_sel, &input);
                    self.report_save(r);
                }
            }
            _ => {}
        }
    }

    fn on_log_key(&mut self, k: KeyEvent) {
        let max = self.shown_log().len();
        self.log_scroll = match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.log_scroll + 1,
            KeyCode::Down | KeyCode::Char('j') => self.log_scroll.saturating_sub(1),
            KeyCode::PageUp => self.log_scroll + 20,
            KeyCode::PageDown => self.log_scroll.saturating_sub(20),
            KeyCode::Home => max,
            KeyCode::End | KeyCode::Char('f') => 0,
            KeyCode::Char('w') => {
                self.log_filter = !self.log_filter;
                0
            }
            _ => self.log_scroll,
        }
        .min(max);
    }

    /// The log lines the tab shows: every line, or warnings and errors only.
    #[must_use]
    pub fn shown_log(&self) -> Vec<&str> {
        self.log
            .iter()
            .map(String::as_str)
            .filter(|l| !self.log_filter || is_warn_or_error(l))
            .collect()
    }

    fn on_picker_key(&mut self, k: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => {
                self.picker = None;
                self.say("Cancelled; nothing changed.", Tone::Info);
            }
            KeyCode::Up => p.step(-1),
            KeyCode::Down => p.step(1),
            KeyCode::PageUp => p.step(-10),
            KeyCode::PageDown => p.step(10),
            KeyCode::Home => p.jump(false),
            KeyCode::End => p.jump(true),
            KeyCode::Char(' ') => p.toggle(),
            KeyCode::Backspace => p.backspace(),
            KeyCode::Enter => {
                let (field, chosen) = (p.field, p.chosen());
                self.picker = None;
                let Some(Ok(doc)) = self.config.as_mut() else {
                    return;
                };
                let same = doc.fields().get(field).is_some_and(|f| {
                    crate::config_edit::list_order(&f.options, &f.values)
                        == crate::config_edit::list_order(&f.options, &chosen)
                });
                if same {
                    self.say("Nothing changed.", Tone::Info);
                } else {
                    let r = doc.set_list(field, &chosen);
                    self.report_save(r);
                }
            }
            KeyCode::Char(c) if !c.is_control() => p.type_char(c),
            _ => {}
        }
    }

    fn on_reading_key(&mut self, k: KeyEvent, at: usize) {
        const PAGE: usize = 10;
        self.reading = match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => None,
            KeyCode::Up | KeyCode::Char('k') => Some(at.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => Some(at.saturating_add(1)),
            KeyCode::PageUp => Some(at.saturating_sub(PAGE)),
            KeyCode::PageDown => Some(at.saturating_add(PAGE)),
            KeyCode::Home => Some(0),
            KeyCode::End => Some(usize::MAX),
            _ => Some(at),
        }
        .map(|at| at.min(super::ui::reading_max(self)));
    }

    fn on_loader_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.loader_sel = self.loader_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.loader_sel = (self.loader_sel + 1).min(1),
            KeyCode::Left | KeyCode::Char('h') => self.change_setting(-1),
            KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::Enter => {
                self.change_setting(1);
            }
            _ => {}
        }
    }

    fn move_mod(&mut self, delta: isize) {
        if self.mods.is_empty() {
            return;
        }
        let n = self.mods.len() as isize;
        self.mod_sel = (self.mod_sel as isize + delta).rem_euclid(n) as usize;
        self.cfg_sel = 0;
        self.load_config();
    }

    /// A saved setting: "Config saved.", plus what the save brought up to
    /// date in an older file.
    fn report_save(&mut self, r: Result<Option<String>, String>) {
        match r {
            Ok(None) => self.say_saved("Config saved."),
            Ok(Some(notice)) => self.say_saved(&format!("Config saved; {notice}.")),
            Err(e) => {
                // The file by its name: the full path would not fit the line.
                let e = match &self.config {
                    Some(Ok(doc)) => crate::config_edit::by_file_name(&e, &doc.path),
                    _ => e,
                };
                self.say(e, Tone::Bad);
            }
        }
    }

    /// The loader settings as they are on disk now (a save changes only its
    /// own key: merge on write). `Err` for a malformed file, left alone.
    fn reread_settings(&mut self, paths: &Paths) -> bool {
        match ndml_common::read_checked(&paths.settings_file()) {
            Ok((s, _)) => {
                self.settings = s;
                true
            }
            Err(e) => {
                self.say(format!("{e}; nothing changed"), Tone::Bad);
                false
            }
        }
    }

    fn toggle_mod(&mut self) {
        let (Some(paths), Some(m)) = (self.paths.clone(), self.selected_mod().cloned()) else {
            return;
        };
        if !self.reread_settings(&paths) {
            return;
        }
        let enabled = !self
            .settings
            .disabled_mods
            .iter()
            .any(|d| d.eq_ignore_ascii_case(&m.stem));
        let r = deploy::set_enabled(&paths, &mut self.settings, &m.stem, !enabled);
        let msg = format!(
            "{} {}.",
            m.stem,
            if enabled { "disabled" } else { "enabled" }
        );
        if self.report(r, &msg) {
            self.mods[self.mod_sel].enabled = !enabled;
        }
    }

    fn adjust(&mut self, steps: i32) {
        if let Some(Ok(doc)) = self.config.as_mut() {
            match doc.adjust(self.cfg_sel, steps) {
                Ok(Adjusted::Saved(notice)) => self.report_save(Ok(notice)),
                Ok(Adjusted::NotAdjustable) => {
                    self.say("Press Enter to edit this value.", Tone::Info);
                }
                Err(e) => self.say(e, Tone::Bad),
            }
        }
    }

    fn change_setting(&mut self, dir: i32) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        if !self.reread_settings(&paths) {
            return;
        }
        match self.loader_sel {
            0 => {
                let cur = LOG_LEVELS
                    .iter()
                    .position(|l| *l == self.settings.log_level)
                    .unwrap_or(2) as i32;
                let next = (cur + dir).rem_euclid(LOG_LEVELS.len() as i32) as usize;
                LOG_LEVELS[next].clone_into(&mut self.settings.log_level);
            }
            _ => self.settings.console = !self.settings.console,
        }
        let r = std::fs::create_dir_all(&paths.root)
            .map_err(|e| e.to_string())
            .and_then(|()| ndml_common::save(&paths.settings_file(), &self.settings));
        self.report(r, "Loader settings saved.");
    }

    fn install(&mut self) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        match deploy::install(&paths, &self.build) {
            Ok(done) => {
                let (msg, tone) = install_message(&done);
                self.say(msg, tone);
            }
            Err(e) => self.say(e, Tone::Bad),
        }
        self.refresh();
    }

    fn launch(&mut self) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        if self.running == Running::Yes {
            self.say("Nova Drift is already running.", Tone::Warn);
            return;
        }
        if self.loader == LoaderState::Missing {
            self.say(
                "Loader not installed — press [I] first (launching unmodded).",
                Tone::Warn,
            );
        }
        match game::launch(&paths.game) {
            Ok(()) => self.say("Launching Nova Drift… watch the LOG tab.", Tone::Good),
            Err(e) => self.say(format!("launch failed: {e}"), Tone::Bad),
        }
    }

    fn run_confirmed(&mut self, c: Confirm) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        match c {
            Confirm::Uninstall => {
                let r = deploy::uninstall(&paths);
                self.report(r, "Loader removed. The game will start unmodded.");
            }
            Confirm::RemoveMod => {
                if let Some(m) = self.selected_mod().cloned() {
                    let r = deploy::remove_mod(&m);
                    self.report(r, &format!("{} removed from the game.", m.stem));
                }
            }
            Confirm::ResetConfig => {
                if let Some(Ok(doc)) = self.config.as_mut() {
                    let r = doc.reset();
                    self.report(r, "Config reset to defaults.");
                }
            }
        }
        self.refresh();
    }

    /// Count of mods that will load next launch.
    #[must_use]
    pub fn active_mods(&self) -> usize {
        self.mods
            .iter()
            .filter(|m| m.enabled && m.deployed.is_some())
            .count()
    }

    #[must_use]
    pub fn needs_deploy(&self) -> bool {
        matches!(
            self.loader,
            LoaderState::Missing | LoaderState::Installed { up_to_date: false }
        ) || self
            .mods
            .iter()
            .any(|m| matches!(m.state, ModState::Outdated | ModState::NotDeployed))
    }
}

/// The status line after an install: what was deployed, and the settings
/// files in counts (`settings updated: magnet-range (1 added)`); amber
/// when a settings file was left as it was. The full lines are what
/// `ndml install` prints.
#[must_use]
pub fn install_message(done: &deploy::Installed) -> (String, Tone) {
    // The loader line is not a mod.
    let n = done.deployed.len().saturating_sub(1);
    let head = format!("Deployed loader + {n} mod(s); ");
    // The status line's room: the frame less its " » " marker.
    let room = usize::from(super::theme::WIDTH) - 3 - head.chars().count() - 1;
    match done.settings_summary(room) {
        None => (
            format!("Deployed loader + {n} mod(s). Launch with [G]."),
            Tone::Good,
        ),
        Some((s, false)) => (format!("{head}{s}."), Tone::Good),
        Some((s, true)) => (format!("{head}{s}."), Tone::Warn),
    }
}

/// A log line (`[  12.345] LEVEL target: message`) at WARN or ERROR.
#[must_use]
pub fn is_warn_or_error(line: &str) -> bool {
    line.split_once("] ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|level| level == "WARN" || level == "ERROR")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log tab's lines come from the log's end only: the same last
    /// lines as reading it whole, without the line a cut splits.
    #[test]
    fn the_log_is_read_from_its_end() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("log-tail-{}.log", std::process::id()));
        let short = "[    0.012] INFO  ndml: one\n[    0.013] WARN  ndml: two\n";
        std::fs::write(&path, short).unwrap();
        assert_eq!(log_tail(&path).unwrap(), short.lines().collect::<Vec<_>>());
        // Longer than the part read: the last lines, whole.
        let line = |i: usize| format!("[{i:>9}] INFO  ndml: {}", "x".repeat(600));
        let text: String = (0..2500).map(|i| line(i) + "\n").collect();
        assert!(text.len() as u64 > LOG_TAIL_BYTES);
        std::fs::write(&path, &text).unwrap();
        let tail = log_tail(&path).unwrap();
        let all: Vec<&str> = text.lines().collect();
        assert!(!tail.is_empty() && tail.len() < LOG_TAIL);
        assert_eq!(tail.last().map(String::as_str), all.last().copied());
        assert!(
            tail.iter().all(|l| all.contains(&l.as_str())),
            "no cut line"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_warnings_filter_keeps_warn_and_error_lines() {
        assert!(is_warn_or_error("[    1.204] WARN  ndmod_magnet_range: x"));
        assert!(is_warn_or_error("[    3.010] ERROR ndmod_probe: x"));
        assert!(!is_warn_or_error("[    0.012] INFO  ndml: WARN x"));
        assert!(!is_warn_or_error("no level here"));
    }
}
