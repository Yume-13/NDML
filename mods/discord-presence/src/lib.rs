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
//!   through the global instance, not `self`). See [`read_globals`] and
//!   `docs/how-it-works.md`.
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
use presence::{ElapsedClock, Templates, Toggles};
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

/// Nova Drift's own Discord application (Discord's public detectable-applications
/// list, entry "Nova Drift", executable `nova drift/novadrift.exe`).
/// The shipped `config.toml` default for `client_id`.
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
    pub client_id: String,
    pub show_level: bool,
    pub show_wave: bool,
    pub show_score: bool,
    pub show_time: bool,
    pub show_boss: bool,
    pub show_hull: bool,
    pub show_paused: bool,
    pub show_elapsed: bool,
    pub show_in_menu: bool,
    pub run_details: String,
    pub run_state: String,
    pub paused_details: String,
    pub paused_state: String,
    pub game_over_details: String,
    pub game_over_state: String,
    pub menu_details: String,
    pub menu_state: String,
    pub large_image: String,
    pub large_text: String,
    pub thousands_separator: String,
    pub update_interval_secs: u64,
    pub boss_linger_secs: u64,
}

impl Config {
    /// The client id if it looks like a Discord snowflake (digits only).
    #[must_use]
    pub fn valid_client_id(&self) -> Option<&str> {
        let id = self.client_id.trim();
        (!id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
    }

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
            paused_details: self.paused_details.clone(),
            paused_state: self.paused_state.clone(),
            game_over_details: self.game_over_details.clone(),
            game_over_state: self.game_over_state.clone(),
            menu_details: self.menu_details.clone(),
            menu_state: self.menu_state.clone(),
            show_in_menu: self.show_in_menu,
            show_elapsed: self.show_elapsed,
            large_image: self.large_image.clone(),
            large_text: self.large_text.clone(),
            thousands_separator: self.thousands_separator.clone(),
        }
    }
}

/// Mod state shared by the hooks (all on the game thread).
#[derive(Debug)]
struct State {
    tracker: Tracker,
    clock: ElapsedClock,
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        let start = match snap.run_secs {
            Some(secs) if snap.phase != presence::Phase::Menu => {
                Some(self.clock.start_for(unix_now(), secs))
            }
            _ => {
                self.clock.reset();
                None
            }
        };
        let desired: Desired =
            presence::activity(&snap.masked(self.toggles), &self.templates, start)
                .map(|a| presence::activity_json(&a));
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
/// pauses doesn't make the elapsed counter jump back on resume.
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
            log::warn!("{e}; that part of the presence is unavailable");
            false
        }
    }
}

fn init(ctx: &ModContext) -> Result<(), Error> {
    let config: Config = ctx.load_config()?;
    if !config.enabled {
        log::info!("disabled in config");
        return Ok(());
    }
    let Some(client_id) = config.valid_client_id() else {
        log::warn!(
            "no valid client_id in {}.toml (default: Nova Drift's app {NOVA_DRIFT_APP_ID}, \
             or a custom app ID from https://discord.com/developers/applications); mod idle",
            ctx.name()
        );
        return Ok(());
    };
    let worker = Worker::spawn(client_id.to_owned(), config.update_interval())
        .map_err(|e| Error::Other(format!("cannot start Discord thread: {e}")))?;
    let state: Shared = Arc::new(Mutex::new(State {
        tracker: Tracker::default(),
        clock: ElapsedClock::default(),
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
                if let Some(c) = controller {
                    if s.tracker.on_controller(now, c) {
                        log::debug!("new run (timeGameStart {:?})", c.run_id);
                    }
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
            log::warn!("{e}; hull unavailable");
        }
    }
    hook_state(ctx, STAR_EATER_STEP, &state, |s, now| {
        s.tracker.on_star_eater_heartbeat(now);
    });
    let mut boss_hooks = 0;
    for (i, boss) in BOSSES.iter().enumerate() {
        if hook_state(ctx, &game::create_event(boss), &state, move |s, now| {
            s.tracker.on_boss_created(now, i);
        }) {
            boss_hooks += 1;
        }
        if boss.has_destroy
            && hook_state(ctx, &game::destroy_event(boss), &state, move |s, _| {
                s.tracker.on_boss_destroyed(i);
            })
        {
            boss_hooks += 1;
        }
    }
    log::info!("Discord presence on (client {client_id}, {boss_hooks} boss hooks)");
    Ok(())
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
        assert_eq!(cfg.valid_client_id(), Some(NOVA_DRIFT_APP_ID));
        assert_eq!(cfg.update_interval(), Duration::from_secs(5));
        assert_eq!(cfg.boss_linger_secs, 10);
        assert!(cfg.run_state.contains("{boss}"));
    }

    #[test]
    fn client_id_validation() {
        let with = |id: &str| Config {
            client_id: id.into(),
            ..defaults()
        };
        assert_eq!(
            with(" 123456789012345678 ").valid_client_id(),
            Some("123456789012345678")
        );
        assert_eq!(with("abc").valid_client_id(), None);
        assert_eq!(with("").valid_client_id(), None);
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
        let a = presence::activity(&snap.masked(cfg.toggles()), &cfg.templates(), Some(1)).unwrap();
        assert_eq!(a.details.as_deref(), Some("Wave 40 · Level 18"));
        assert_eq!(a.state.as_deref(), Some("vs Dreadnought · Score 2,500,000"));
        assert_eq!(a.large_image, None);

        // No boss: the hull takes that slot.
        let calm = presence::Snapshot { boss: None, ..snap };
        let a = presence::activity(&calm.masked(cfg.toggles()), &cfg.templates(), Some(1)).unwrap();
        assert_eq!(a.state.as_deref(), Some("Engineer · Score 2,500,000"));
    }

    #[test]
    fn pause_indicator_is_off_by_default() {
        assert!(!defaults().show_paused);
    }
}
