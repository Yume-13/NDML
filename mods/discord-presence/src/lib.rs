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

//! # Discord Presence
//!
//! Shows the current run on the player's Discord profile: wave, level, score,
//! run time and the boss being fought, with configurable templates.
//!
//! ## How it reads the game (Nova Drift 1.2.38.4)
//! * `oController` Step (one controller per run): `pause`, `gameplay`,
//!   `gameOver`, and the run clock `current_time − timeGameStart − timePauseTotal`
//!   (ms; `timeGameStart = current_time` in its Create).
//! * `oMenu` Step: main menu heartbeat. `oPlayer` Step: ship heartbeat.
//! * Boss `Create_0`/`Destroy_0` events (see [`game::BOSSES`]).
//! * `level`, `wave`, `playerScore` are GML **globals** (compiled code reads them
//!   through the global instance, not `self`). See `read_globals`.
//!
//! ## Threads
//! Hooks run on the game thread and only update a small tracker and, at most
//! once a second, queue the rendered activity on a channel. A background
//! thread ([`worker`]) owns the Discord pipe, rate limiting and reconnects.

#![forbid(unsafe_code)]

pub mod game;
pub mod ipc;
pub mod presence;
pub mod worker;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ndml_sdk::{Call, Error, ModContext, Variable};
use serde::Deserialize;

use game::{BOSSES, Controller, Globals, Tracker};
use presence::{Templates, Toggles};
use worker::{Desired, Update, Worker};

pub const CONTROLLER_STEP: &str = "gml_Object_oController_Step_0";
pub const MENU_STEP: &str = "gml_Object_oMenu_Step_0";
pub const PLAYER_STEP: &str = "gml_Object_oPlayer_Step_0";
pub const STAR_EATER_STEP: &str = "gml_Object_oStarEaterController_Step_0";

/// `oController` instance variables.
pub const VAR_PAUSE: &str = "pause";
/// Chosen hull id, an `oPowers` instance variable (see [`game::HULLS`]).
pub const VAR_BODY_TYPE: &str = "bodyType";
/// `oPowers` step: where `bodyType` is read.
pub const POWERS_STEP: &str = "gml_Object_oPowers_Step_0";
pub const VAR_GAMEPLAY: &str = "gameplay";
pub const VAR_GAME_OVER: &str = "gameOver";
pub const VAR_TIME_GAME_START: &str = "timeGameStart";
pub const VAR_TIME_PAUSE_TOTAL: &str = "timePauseTotal";
/// Built-in (milliseconds since game start).
pub const BUILTIN_CURRENT_TIME: &str = "current_time";

/// Nova Drift's own Discord application (Discord's public detectable-games list,
/// entry "Nova Drift", executable `nova drift/novadrift.exe`). The presence
/// always shows as this application.
pub const NOVA_DRIFT_APP_ID: &str = "1124352634573312211";

/// How often the game thread re-renders the presence.
pub const TICK: Duration = Duration::from_secs(1);
pub const MIN_INTERVAL_SECS: u64 = 5;
/// Cap on `boss_linger_secs`.
pub const MAX_LINGER_SECS: u64 = 120;
pub const MAX_INTERVAL_SECS: u64 = 300;

/// `ndml/config/ndmod_discord_presence.toml`; shipped defaults in `config.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // mirrors the config file
pub struct Config {
    pub enabled: bool,
    pub show_level: bool,
    pub show_wave: bool,
    pub show_score: bool,
    pub show_time: bool,
    pub show_boss: bool,
    pub show_hull: bool,
    pub show_paused: bool,
    pub show_in_menu: bool,
    pub run_details: String,
    pub run_state: String,
    pub run_state_alt: String,
    pub rotate_secs: u64,
    pub paused_details: String,
    pub paused_state: String,
    pub game_over_details: String,
    pub game_over_state: String,
    pub menu_details: String,
    pub menu_state: String,
    pub thousands_separator: String,
    pub update_interval_secs: u64,
    pub boss_linger_secs: u64,
}

impl Config {
    #[must_use]
    pub fn update_interval(&self) -> Duration {
        Duration::from_secs(
            self.update_interval_secs
                .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS),
        )
    }

    #[must_use]
    pub fn toggles(&self) -> Toggles {
        Toggles {
            level: self.show_level,
            wave: self.show_wave,
            score: self.show_score,
            time: self.show_time,
            boss: self.show_boss,
            hull: self.show_hull,
        }
    }

    #[must_use]
    pub fn templates(&self) -> Templates {
        Templates {
            run_details: self.run_details.clone(),
            run_state: self.run_state.clone(),
            run_state_alt: self.run_state_alt.clone(),
            paused_details: self.paused_details.clone(),
            paused_state: self.paused_state.clone(),
            game_over_details: self.game_over_details.clone(),
            game_over_state: self.game_over_state.clone(),
            menu_details: self.menu_details.clone(),
            menu_state: self.menu_state.clone(),
            show_in_menu: self.show_in_menu,
            thousands_separator: self.thousands_separator.clone(),
        }
    }
}

/// Mod state shared by the hooks (all on the game thread).
#[derive(Debug)]
struct State {
    tracker: Tracker,
    started: Instant,
    /// The game session's start in Unix seconds, read once in [`init`] and
    /// sent unchanged with every activity (see [`presence::activity_json`]).
    session_start: Option<u64>,
    rotate: Option<Duration>,
    last_tick: Option<Instant>,
    last_sent: Option<Desired>,
    /// Phase and boss of the last published update (changes are urgent).
    last_key: Option<(presence::Phase, Option<&'static str>)>,
    /// Most recent boss shown, for [`game::linger_boss`].
    last_boss: Option<(&'static str, Instant)>,
    boss_linger: Duration,
    toggles: Toggles,
    templates: Templates,
    /// `show_paused`: false presents a paused run exactly like live play.
    show_paused: bool,
    worker: Worker,
}

type Shared = Arc<Mutex<State>>;

fn with_state(state: &Shared, f: impl FnOnce(&mut State)) {
    f(&mut state.lock().unwrap_or_else(PoisonError::into_inner));
}

impl State {
    /// Re-render at most once per [`TICK`] and queue the result if it changed.
    fn tick(&mut self, now: Instant) {
        if self.last_tick.is_some_and(|t| now.duration_since(t) < TICK) {
            return;
        }
        self.last_tick = Some(now);
        let mut snap = self.tracker.snapshot(now);
        if !self.show_paused && snap.phase == presence::Phase::Paused {
            snap.phase = presence::Phase::Run;
        }
        snap.boss = if matches!(snap.phase, presence::Phase::Run | presence::Phase::Paused) {
            game::linger_boss(snap.boss, &mut self.last_boss, now, self.boss_linger)
        } else {
            self.last_boss = None;
            None
        };
        let alt = self
            .rotate
            .is_some_and(|r| rotation_alt(now.duration_since(self.started), r));
        let desired: Desired = presence::activity(&snap.masked(self.toggles), &self.templates, alt)
            .map(|a| presence::activity_json(&a, self.session_start));
        if self.last_sent.as_ref() != Some(&desired) {
            let key = (snap.phase, snap.boss.filter(|_| self.toggles.boss));
            let urgent = self.last_key != Some(key);
            log::debug!(
                "presence -> {desired:?}{}",
                if urgent { " (urgent)" } else { "" }
            );
            self.worker.publish(Update {
                desired: desired.clone(),
                urgent,
            });
            self.last_sent = Some(desired);
            self.last_key = Some(key);
        }
    }
}

/// Whether the alternate state line is due: every other `period` since start.
fn rotation_alt(elapsed: Duration, period: Duration) -> bool {
    let p = period.as_secs().max(1);
    (elapsed.as_secs() / p) % 2 == 1
}

/// Now in Unix seconds from the system clock; `None` (logged) for a clock
/// before 1970, so no timestamps are sent rather than a wrong one.
fn unix_now_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| log::warn!("unable to read the system clock ({e}); no elapsed time shown"))
        .ok()
}

/// `level`, `wave` and `playerScore` are GML globals, read from the global
/// instance ([`Call::global`]). Missing values make the templates drop those
/// segments.
fn read_globals(call: &Call<'_>, vars: &GlobalVars) -> Globals {
    let Some(global) = call.global() else {
        return Globals::default();
    };
    let read = |v: Option<Variable>| game::as_count(v.and_then(|v| global.get_f64(v)));
    Globals {
        level: read(vars.level),
        wave: read(vars.wave),
        score: read(vars.score),
    }
}

/// Resolved global variable handles (kept for [`read_globals`]).
#[derive(Debug, Clone, Copy)]
struct GlobalVars {
    level: Option<Variable>,
    wave: Option<Variable>,
    score: Option<Variable>,
}

#[derive(Debug, Clone, Copy)]
struct ControllerVars {
    pause: Variable,
    gameplay: Variable,
    game_over: Variable,
    time_game_start: Variable,
    time_pause_total: Variable,
}

/// `count_pauses`: subtract paused time from the run clock (the game's own run
/// time). Without it the clock is wall time since the run started, so hiding
/// pauses doesn't make `{time}` jump back on resume.
fn read_controller(call: &Call<'_>, v: &ControllerVars, count_pauses: bool) -> Option<Controller> {
    let me = call.instance()?;
    let flag = |var| me.get_f64(var).is_some_and(|x| x != 0.0);
    let start = me.get_f64(v.time_game_start);
    let paused_ms = if count_pauses {
        me.get_f64(v.time_pause_total).unwrap_or(0.0)
    } else {
        0.0
    };
    Some(Controller {
        pause: flag(v.pause),
        gameplay: flag(v.gameplay),
        game_over: flag(v.game_over),
        run_id: start,
        run_secs: game::run_secs(me.builtin_f64(BUILTIN_CURRENT_TIME), start, paused_ms),
    })
}

/// Register a hook that only touches the shared state; failures are logged,
/// not fatal (that piece of information is just missing then).
fn hook_state(
    ctx: &ModContext,
    function: &str,
    state: &Shared,
    f: impl Fn(&mut State, Instant) + Send + Sync + 'static,
) -> bool {
    let state = Arc::clone(state);
    match ctx.hook_after(function, move |_| {
        with_state(&state, |s| f(s, Instant::now()));
    }) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("unable to track part of the run ({e})");
            false
        }
    }
}

fn init(ctx: &ModContext) -> Result<(), Error> {
    let config: Config = ctx.load_config()?;
    if !config.enabled {
        log::info!("off: enabled = false");
        return Ok(());
    }
    let worker = Worker::spawn(NOVA_DRIFT_APP_ID.to_owned(), config.update_interval())
        .map_err(|e| Error::Other(format!("cannot start Discord thread: {e}")))?;
    let state: Shared = Arc::new(Mutex::new(State {
        tracker: Tracker::default(),
        started: Instant::now(),
        session_start: unix_now_secs(),
        rotate: (config.rotate_secs > 0).then(|| {
            Duration::from_secs(
                config
                    .rotate_secs
                    .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS),
            )
        }),
        last_tick: None,
        last_sent: None,
        last_key: None,
        last_boss: None,
        boss_linger: Duration::from_secs(config.boss_linger_secs.min(MAX_LINGER_SECS)),
        toggles: config.toggles(),
        templates: config.templates(),
        show_paused: config.show_paused,
        worker,
    }));

    let cvars = ControllerVars {
        pause: ctx.variable(VAR_PAUSE)?,
        gameplay: ctx.variable(VAR_GAMEPLAY)?,
        game_over: ctx.variable(VAR_GAME_OVER)?,
        time_game_start: ctx.variable(VAR_TIME_GAME_START)?,
        time_pause_total: ctx.variable(VAR_TIME_PAUSE_TOTAL)?,
    };
    let gvars = GlobalVars {
        level: ctx.variable("level").ok(),
        wave: ctx.variable("wave").ok(),
        score: ctx.variable("playerScore").ok(),
    };
    let count_pauses = config.show_paused;
    {
        let state = Arc::clone(&state);
        ctx.hook_after(CONTROLLER_STEP, move |call| {
            let controller = read_controller(call, &cvars, count_pauses);
            let globals = read_globals(call, &gvars);
            with_state(&state, |s| {
                let now = Instant::now();
                if let Some(c) = controller
                    && s.tracker.on_controller(now, c)
                {
                    log::debug!("new run (timeGameStart {:?})", c.run_id);
                }
                s.tracker.globals = globals;
                s.tick(now);
            });
        })?;
    }
    hook_state(ctx, MENU_STEP, &state, |s, now| {
        s.tracker.on_menu(now);
        s.tick(now);
    });
    hook_state(ctx, PLAYER_STEP, &state, |s, now| s.tracker.on_player(now));
    if let Ok(body_type) = ctx.variable(VAR_BODY_TYPE) {
        let state = Arc::clone(&state);
        let hooked = ctx.hook_after(POWERS_STEP, move |call| {
            let hull = call
                .instance()
                .and_then(|me| game::hull_name(me.get_f64(body_type)));
            with_state(&state, |s| s.tracker.hull = hull);
        });
        if let Err(e) = hooked {
            log::warn!("unable to track the hull ({e})");
        }
    }
    hook_state(ctx, STAR_EATER_STEP, &state, |s, now| {
        s.tracker.on_star_eater_heartbeat(now);
    });
    let bosses = hook_bosses(ctx, &state);
    log::info!("ready: {bosses} bosses tracked");
    Ok(())
}

/// Hook every boss's Create (and Destroy, where it has one). Returns the
/// number of bosses tracked (their Create hooked).
fn hook_bosses(ctx: &ModContext, state: &Shared) -> usize {
    let mut n = 0;
    for (i, boss) in BOSSES.iter().enumerate() {
        if hook_state(ctx, &game::create_event(boss), state, move |s, now| {
            s.tracker.on_boss_created(now, i);
        }) {
            n += 1;
        }
        if boss.has_destroy {
            hook_state(ctx, &game::destroy_event(boss), state, move |s, _| {
                s.tracker.on_boss_destroyed(i);
            });
        }
    }
    n
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Config {
        ndml_sdk::config::parse_defaults(NDML_DEFAULT_CONFIG).unwrap()
    }

    #[test]
    fn shipped_defaults_parse() {
        let cfg = defaults();
        assert!(cfg.enabled);
        assert_eq!(cfg.update_interval(), Duration::from_secs(5));
        assert_eq!(cfg.boss_linger_secs, 10);
        assert!(cfg.run_state.contains("{boss}"));
    }

    /// A Discord application id is a snowflake: digits only.
    #[test]
    fn app_id_is_a_snowflake() {
        assert!(!NOVA_DRIFT_APP_ID.is_empty());
        assert!(NOVA_DRIFT_APP_ID.bytes().all(|b| b.is_ascii_digit()));
    }

    /// A settings file that still holds `client_id`, `large_image` and
    /// `large_text` (keys this mod no longer reads): the settings refresh
    /// drops them and keeps the old file once as `.bak`, every other value
    /// still counts, and a second load changes nothing.
    #[test]
    fn an_old_file_with_retired_keys_loads_without_them() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/discord-presence-old-config");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ndmod_discord_presence.toml");
        let bak = ndml_sdk::config::backup_path(&path);
        let _ = std::fs::remove_file(&bak);
        let old = NDML_DEFAULT_CONFIG
            .replace(
                "show_wave = true",
                "client_id = \"987654321098765432\"\nshow_wave = false",
            )
            .replace("rotate_secs = 20", "rotate_secs = 30")
            .replace(
                "menu_state = \"\"",
                "menu_state = \"\"\n\n# Picture from your own Discord app.\n\
                 large_image = \"logo\"\nlarge_text = \"My art\"",
            );
        let notice = ndml_sdk::config::refresh(&old, NDML_DEFAULT_CONFIG)
            .unwrap()
            .notice();
        assert_eq!(
            notice.as_deref(),
            Some(
                "config refreshed: client_id dropped (was \"987654321098765432\"), \
                 large_image dropped (was \"logo\"), large_text dropped (was \"My art\")"
            )
        );
        std::fs::write(&path, &old).unwrap();

        let c: Config = ndml_sdk::config::load(&path, NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(
            c,
            Config {
                show_wave: false,
                rotate_secs: 30,
                ..defaults()
            }
        );
        let now = std::fs::read_to_string(&path).unwrap();
        for k in ["client_id", "large_image", "large_text"] {
            assert!(!now.contains(k), "{k} retired: dropped\n{now}");
        }
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), old);

        let _: Config = ndml_sdk::config::load(&path, NDML_DEFAULT_CONFIG).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            now,
            "a second load"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interval_is_clamped() {
        let with = |s| Config {
            update_interval_secs: s,
            ..defaults()
        };
        assert_eq!(
            with(0).update_interval(),
            Duration::from_secs(MIN_INTERVAL_SECS)
        );
        assert_eq!(
            with(9999).update_interval(),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
    }

    #[test]
    fn default_templates_render_a_boss_fight() {
        let cfg = defaults();
        let snap = presence::Snapshot {
            phase: presence::Phase::Run,
            level: Some(18),
            wave: Some(40),
            score: Some(2_500_000),
            run_secs: Some(1500),
            boss: Some("Dreadnought"),
            hull: Some("Engineer"),
        };
        let a = presence::activity(&snap.masked(cfg.toggles()), &cfg.templates(), false).unwrap();
        assert_eq!(a.details.as_deref(), Some("Wave 40 · Level 18"));
        assert_eq!(a.state.as_deref(), Some("vs Dreadnought · Score 2,500,000"));
        assert!(
            !presence::activity_json(&a, Some(1_750_000_000)).contains("assets"),
            "no picture"
        );

        // No boss: the hull takes that slot.
        let calm = presence::Snapshot { boss: None, ..snap };
        let a = presence::activity(&calm.masked(cfg.toggles()), &cfg.templates(), false).unwrap();
        assert_eq!(a.state.as_deref(), Some("Engineer · Score 2,500,000"));
    }

    #[test]
    fn state_line_rotates_between_score_and_run_time() {
        let cfg = defaults();
        assert_eq!(cfg.rotate_secs, 20);
        let p = Duration::from_secs(cfg.rotate_secs);
        assert!(!rotation_alt(Duration::from_secs(5), p));
        assert!(rotation_alt(Duration::from_secs(25), p));
        assert!(!rotation_alt(Duration::from_secs(45), p));
        let snap = presence::Snapshot {
            phase: presence::Phase::Run,
            level: Some(18),
            wave: Some(40),
            score: Some(2_500_000),
            run_secs: Some(754),
            boss: None,
            hull: Some("Engineer"),
        };
        let t = cfg.templates();
        let a = presence::activity(&snap, &t, false).unwrap();
        assert_eq!(a.state.as_deref(), Some("Engineer · Score 2,500,000"));
        let b = presence::activity(&snap, &t, true).unwrap();
        assert_eq!(b.state.as_deref(), Some("Engineer · Run 12:34"));
        // Both lines carry the same session start: the rotation does not
        // restart the time Discord shows.
        let start = Some(1_750_000_000);
        let ts = r#""timestamps":{"start":1750000000}"#;
        assert!(presence::activity_json(&a, start).contains(ts));
        assert!(presence::activity_json(&b, start).contains(ts));
    }

    #[test]
    fn pause_indicator_is_off_by_default() {
        assert!(!defaults().show_paused);
    }
}

#[cfg(test)]
mod config_keys {
    /// The shipped `config.toml` lists exactly the keys `Config` reads: a
    /// settings refresh drops what the shipped file lacks, so a key read but
    /// not shipped would be dropped from every player's file.
    #[test]
    fn shipped_config_lists_every_key() {
        ndml_sdk::config::check_shipped_keys::<super::Config>(super::NDML_DEFAULT_CONFIG, &[])
            .unwrap();
    }
}
