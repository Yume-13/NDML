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

//! TUI state and input handling (no drawing here; see `ui.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ndml_common::{Paths, Settings};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::fx::Starfield;
use crate::config_edit::ConfigDoc;
use crate::deploy::{self, LoaderState, ModEntry, ModState};
use crate::game;

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
pub const SHOW_SAFETY_NOTICE: bool = true;

/// Text of the startup safety notice, one entry per line.
pub const SAFETY_NOTICE: [&str; 5] = [
    "NDML works fully offline: it never needs or uses internet access.",
    "Its complete source code should be included with this download.",
    "",
    "Mods are native programs with the same access as the game itself.",
    "Only install mods from sources you trust.",
];
const LOG_TAIL: usize = 2000;

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
    pub running: bool,
    pub log: Vec<String>,
    /// Lines scrolled up from the bottom of the log.
    pub log_scroll: usize,
    pub loader_sel: usize,
    pub status: Option<(String, Tone, Instant)>,
    pub confirm: Option<Confirm>,
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
            running: false,
            log: Vec::new(),
            log_scroll: 0,
            loader_sel: 0,
            status: None,
            confirm: None,
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

    fn report<T>(&mut self, result: Result<T, String>, ok_msg: &str) -> bool {
        match result {
            Ok(_) => {
                let tone = if self.running { Tone::Warn } else { Tone::Good };
                let suffix = if self.running {
                    " (applies on next launch)"
                } else {
                    ""
                };
                self.say(format!("{ok_msg}{suffix}"), tone);
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
        self.settings = ndml_common::read(&paths.settings_file());
        self.mods = deploy::scan_mods(&paths, &self.build, &self.settings);
        self.mod_sel = self.mod_sel.min(self.mods.len().saturating_sub(1));
        self.loader = deploy::loader_state(&paths, &self.build);
        self.load_config();
        self.poll();
    }

    /// Cheap periodic refresh (game process, log tail).
    pub fn poll(&mut self) {
        self.last_poll = Instant::now();
        self.running = self.watcher.get();
        if let Some(paths) = &self.paths {
            if let Ok(text) = std::fs::read_to_string(paths.log_file()) {
                let lines: Vec<&str> = text.lines().collect();
                let start = lines.len().saturating_sub(LOG_TAIL);
                self.log = lines[start..].iter().map(|s| (*s).to_owned()).collect();
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
    /// (used by `ndml snapshot`).
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
                let default = m
                    .meta
                    .as_ref()
                    .map(|i| i.default_config.clone())
                    .unwrap_or_default();
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
        if self.editing.is_some() {
            self.on_edit_key(k);
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
                if let Some(Ok(doc)) = &self.config {
                    if let Some(f) = doc.fields().get(self.cfg_sel) {
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
            KeyCode::Char(c) if buf.len() < 40 => buf.push(c),
            KeyCode::Enter => {
                let input = self.editing.take().unwrap_or_default();
                if let Some(Ok(doc)) = self.config.as_mut() {
                    let r = doc.set_from_str(self.cfg_sel, &input);
                    self.report(r, "Config saved.");
                }
            }
            _ => {}
        }
    }

    fn on_log_key(&mut self, k: KeyEvent) {
        let max = self.log.len();
        self.log_scroll = match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.log_scroll + 1,
            KeyCode::Down | KeyCode::Char('j') => self.log_scroll.saturating_sub(1),
            KeyCode::PageUp => self.log_scroll + 20,
            KeyCode::PageDown => self.log_scroll.saturating_sub(20),
            KeyCode::Home => max,
            KeyCode::End | KeyCode::Char('f') => 0,
            _ => self.log_scroll,
        }
        .min(max);
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

    fn toggle_mod(&mut self) {
        let (Some(paths), Some(m)) = (self.paths.clone(), self.selected_mod().cloned()) else {
            return;
        };
        let r = deploy::set_enabled(&paths, &mut self.settings, &m.stem, !m.enabled);
        let msg = format!(
            "{} {}.",
            m.stem,
            if m.enabled { "disabled" } else { "enabled" }
        );
        if self.report(r, &msg) {
            self.mods[self.mod_sel].enabled = !m.enabled;
        }
    }

    fn adjust(&mut self, steps: i32) {
        if let Some(Ok(doc)) = self.config.as_mut() {
            match doc.adjust(self.cfg_sel, steps) {
                Ok(true) => {
                    let r: Result<(), String> = Ok(());
                    self.report(r, "Config saved.");
                }
                Ok(false) => self.say("Press Enter to edit this value.", Tone::Info),
                Err(e) => self.say(e, Tone::Bad),
            }
        }
    }

    fn change_setting(&mut self, dir: i32) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
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
        let r = deploy::install(&paths, &self.build);
        let n = r.as_ref().map_or(0, Vec::len);
        self.report(
            r,
            &format!(
                "Deployed loader + {} mod(s). Launch with [G].",
                n.saturating_sub(1)
            ),
        );
        self.refresh();
    }

    fn launch(&mut self) {
        let Some(paths) = self.paths.clone() else {
            return;
        };
        if self.running {
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
                let default = self
                    .selected_mod()
                    .and_then(|m| m.meta.as_ref().ok())
                    .map(|i| i.default_config.clone())
                    .unwrap_or_default();
                if let Some(Ok(doc)) = self.config.as_mut() {
                    let r = doc.reset(&default);
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
