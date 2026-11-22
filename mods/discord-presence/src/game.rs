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

//! Game state assembled from several hooks (pure logic).
//!
//! Game facts this relies on (Nova Drift 1.2.38.4):
//! * `oController` (one per run) owns `pause`, `gameplay`, `gameOver`,
//!   `timeGameStart`, `timePauseTotal` as instance variables.
//! * `oMenu`'s Step runs every frame on the main menu.
//! * Bosses are objects; each has its own `Create_0`, and all but `oStarEater`
//!   have their own `Destroy_0`. The Star Eater fight is driven by
//!   `oStarEaterController`, whose Step serves as a heartbeat instead.
//! * The chosen hull is `oPowers.bodyType` (see [`hull_name`]).

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use crate::presence::{Phase, Snapshot};

/// A boss object and the name the game shows for it (`EnemyName*` strings).
#[derive(Clone, Copy, Debug)]
pub struct Boss {
    pub object: &'static str,
    pub name: &'static str,
    /// Has its own `Destroy_0` (otherwise tracked by heartbeat).
    pub has_destroy: bool,
}

const fn boss(object: &'static str, name: &'static str) -> Boss {
    Boss {
        object,
        name,
        has_destroy: true,
    }
}

/// Index of the Star Eater in [`BOSSES`].
pub const STAR_EATER: usize = 10;

pub const BOSSES: [Boss; 11] = [
    boss("oSpitfire", "Spitfire"),
    boss("oStationCore", "Station Omega"),
    boss("oDreadnought", "Dreadnought"),
    boss("oScion", "Scion"),
    boss("oDweller", "Dweller"),
    boss("oWarbringer", "Warbringer"),
    boss("oSeraph", "Seraph"),
    boss("oCeratotitan", "Ceratotitan"),
    boss("oGlaucus", "Glaucus"),
    boss("oCrucible", "Crucible"),
    Boss {
        object: "oStarEater",
        name: "Star Eater",
        has_destroy: false,
    },
];

/// Hull display names by `oPowers.bodyType`. From `assignBaseStats`'s
/// `switch (bodyType)`: each case's block reads `<hull>BaseThrust`/`BaseHull`
/// (case 1 has no prefix: the Standard hull); names are the game's
/// `GearTitleBody*` strings (the Stealth key displays as "Spectre").
pub const HULLS: [(i64, &str); 14] = [
    (1, "Standard"),
    (2, "Assault"),
    (3, "Hullbreaker"),
    (4, "Spectre"),
    (5, "Sentinel"),
    (6, "Battery"),
    (7, "Engineer"),
    (8, "Research"),
    (9, "Architect"),
    (10, "Viper"),
    (11, "Carrier"),
    (12, "Courser"),
    (13, "Firefly"),
    (14, "Leviathan"),
];

/// Display name for a `bodyType` value, if known.
#[must_use]
pub fn hull_name(body_type: Option<f64>) -> Option<&'static str> {
    let id = as_count(body_type)?;
    HULLS.iter().find(|(n, _)| *n == id).map(|(_, name)| *name)
}

/// Event function names for a boss.
#[must_use]
pub fn create_event(b: &Boss) -> String {
    format!("gml_Object_{}_Create_0", b.object)
}

#[must_use]
pub fn destroy_event(b: &Boss) -> String {
    format!("gml_Object_{}_Destroy_0", b.object)
}

/// Keep showing the last boss for `linger` after it's gone, so a fight that
/// is over in seconds still reaches Discord. Pure:
/// `last` remembers the most recent boss shown and when.
pub fn linger_boss(
    current: Option<&'static str>,
    last: &mut Option<(&'static str, Instant)>,
    now: Instant,
    linger: Duration,
) -> Option<&'static str> {
    if let Some(b) = current {
        *last = Some((b, now));
        return Some(b);
    }
    match *last {
        Some((b, at)) if now.saturating_duration_since(at) < linger => Some(b),
        _ => {
            *last = None;
            None
        }
    }
}

/// A heartbeat older than this means the object is gone (or never ran).
pub const STALE: Duration = Duration::from_secs(2);
/// Sanity bound for a computed run time (100 hours).
pub const MAX_RUN_SECS: u64 = 100 * 3600;

/// `oController` state read in its Step event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Controller {
    pub pause: bool,
    pub gameplay: bool,
    pub game_over: bool,
    /// `timeGameStart`: changes when a new run starts (or a save is loaded).
    pub run_id: Option<f64>,
    pub run_secs: Option<u64>,
}

/// GML globals (`level`, `wave`, `playerScore`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Globals {
    pub level: Option<i64>,
    pub wave: Option<i64>,
    pub score: Option<i64>,
}

/// Run time from the controller's clock values (all in `current_time`
/// milliseconds). `None` if they are missing or implausible.
#[must_use]
pub fn run_secs(current_ms: Option<f64>, start_ms: Option<f64>, paused_ms: f64) -> Option<u64> {
    let ms = current_ms? - start_ms? - paused_ms;
    if !ms.is_finite() || ms < 0.0 {
        return None;
    }
    // The cast takes a value checked finite and >= 0.
    let secs = (ms / 1000.0) as u64;
    (secs <= MAX_RUN_SECS).then_some(secs)
}

/// Rounded integer view of a game number, if sane.
#[must_use]
pub fn as_count(v: Option<f64>) -> Option<i64> {
    let v = v?;
    // The cast takes a range-checked value.
    (v.is_finite() && v.abs() < 1e15).then(|| v.round() as i64)
}

fn fresh(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|t| now.saturating_duration_since(t) <= STALE)
}

/// Everything the hooks have reported so far.
#[derive(Debug, Default)]
pub struct Tracker {
    controller: Option<(Instant, Controller)>,
    player_at: Option<Instant>,
    menu_at: Option<Instant>,
    star_eater_at: Option<Instant>,
    alive: [u32; BOSSES.len()],
    /// Boss indices in spawn order (most recent last).
    spawned: Vec<usize>,
    run_id: Option<f64>,
    pub globals: Globals,
    /// Chosen hull, from `oPowers.bodyType`.
    pub hull: Option<&'static str>,
}

impl Tracker {
    /// `oController` Step. Returns `true` if this looks like a new run.
    pub fn on_controller(&mut self, now: Instant, c: Controller) -> bool {
        let new_run = c.run_id.is_some() && c.run_id != self.run_id;
        if new_run {
            self.run_id = c.run_id;
            self.clear_bosses();
        }
        self.controller = Some((now, c));
        new_run
    }

    pub fn on_player(&mut self, now: Instant) {
        self.player_at = Some(now);
    }

    pub fn on_menu(&mut self, now: Instant) {
        self.menu_at = Some(now);
        self.clear_bosses();
    }

    pub fn on_boss_created(&mut self, now: Instant, i: usize) {
        if let Some(n) = self.alive.get_mut(i) {
            *n += 1;
            self.spawned.retain(|&b| b != i);
            self.spawned.push(i);
            if i == STAR_EATER {
                self.star_eater_at = Some(now);
            }
        }
    }

    pub fn on_boss_destroyed(&mut self, i: usize) {
        if let Some(n) = self.alive.get_mut(i) {
            *n = n.saturating_sub(1);
        }
    }

    pub fn on_star_eater_heartbeat(&mut self, now: Instant) {
        self.star_eater_at = Some(now);
    }

    fn clear_bosses(&mut self) {
        self.alive = [0; BOSSES.len()];
        self.spawned.clear();
    }

    /// Current phase.
    #[must_use]
    pub fn phase(&self, now: Instant) -> Phase {
        if fresh(self.menu_at, now) {
            return Phase::Menu;
        }
        let Some((at, c)) = self.controller else {
            return Phase::Menu;
        };
        if !fresh(Some(at), now) {
            return Phase::Menu;
        }
        if c.game_over {
            Phase::GameOver
        } else if c.pause {
            Phase::Paused
        } else if fresh(self.player_at, now) || !c.gameplay {
            // `gameplay` is false on the upgrade screen, where the ship may idle.
            Phase::Run
        } else {
            // A controller without a ship: loadout or an unrecognised screen.
            Phase::Menu
        }
    }

    /// The boss being fought, if any: the most recently spawned one still alive.
    #[must_use]
    pub fn boss(&self, now: Instant) -> Option<&'static str> {
        let phase = self.phase(now);
        if !matches!(phase, Phase::Run | Phase::Paused) {
            return None;
        }
        self.spawned
            .iter()
            .rev()
            .copied()
            .find(|&i| {
                self.alive[i] > 0
                    && (BOSSES[i].has_destroy
                        // Steps may not run while paused; keep the last known state.
                        || phase == Phase::Paused
                        || fresh(self.star_eater_at, now))
            })
            .map(|i| BOSSES[i].name)
    }

    #[must_use]
    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let phase = self.phase(now);
        let in_run = phase != Phase::Menu;
        let c = self.controller.map_or_default(|(_, c)| c);
        Snapshot {
            phase,
            level: self.globals.level.filter(|_| in_run),
            wave: self.globals.wave.filter(|_| in_run),
            score: self.globals.score.filter(|_| in_run),
            run_secs: c.run_secs.filter(|_| in_run),
            boss: self.boss(now),
            hull: self.hull.filter(|_| in_run),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defeated_boss_lingers_then_clears() {
        let t0 = Instant::now();
        let linger = Duration::from_secs(10);
        let mut last = None;
        assert_eq!(
            linger_boss(Some("Seraph"), &mut last, t0, linger),
            Some("Seraph")
        );
        // Killed two seconds in: still shown for the linger time.
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(linger_boss(None, &mut last, t1, linger), Some("Seraph"));
        let t2 = t0 + Duration::from_secs(11);
        assert_eq!(linger_boss(None, &mut last, t2, linger), None);
        assert_eq!(linger_boss(None, &mut last, t0, linger), None, "cleared");
        // A new boss replaces a lingering one immediately.
        let mut last = Some(("Seraph", t0));
        assert_eq!(
            linger_boss(Some("Scion"), &mut last, t1, linger),
            Some("Scion")
        );
        // linger = 0 disables it.
        let mut last = Some(("Seraph", t0));
        assert_eq!(linger_boss(None, &mut last, t0, Duration::ZERO), None);
    }

    #[test]
    fn hull_names_by_body_type() {
        assert_eq!(hull_name(Some(7.0)), Some("Engineer"));
        assert_eq!(hull_name(Some(1.0)), Some("Standard"));
        assert_eq!(hull_name(Some(4.0)), Some("Spectre"));
        assert_eq!(hull_name(Some(14.0)), Some("Leviathan"));
        assert_eq!(hull_name(Some(0.0)), None);
        assert_eq!(hull_name(Some(99.0)), None);
        assert_eq!(hull_name(None), None);
    }

    fn ctl(run: f64) -> Controller {
        Controller {
            gameplay: true,
            run_id: Some(run),
            run_secs: Some(60),
            ..Controller::default()
        }
    }

    #[test]
    fn boss_table_matches_star_eater_index() {
        assert_eq!(BOSSES[STAR_EATER].object, "oStarEater");
        assert!(!BOSSES[STAR_EATER].has_destroy);
        assert_eq!(BOSSES.iter().filter(|b| !b.has_destroy).count(), 1);
        assert_eq!(create_event(&BOSSES[0]), "gml_Object_oSpitfire_Create_0");
        assert_eq!(
            destroy_event(&BOSSES[1]),
            "gml_Object_oStationCore_Destroy_0"
        );
    }

    #[test]
    fn run_time_math() {
        assert_eq!(run_secs(Some(70_000.0), Some(5_000.0), 5_000.0), Some(60));
        assert_eq!(run_secs(Some(1_000.0), Some(5_000.0), 0.0), None);
        assert_eq!(run_secs(None, Some(0.0), 0.0), None);
        assert_eq!(run_secs(Some(f64::NAN), Some(0.0), 0.0), None);
        assert_eq!(run_secs(Some(1e12), Some(0.0), 0.0), None);
        assert_eq!(as_count(Some(12.0)), Some(12));
        assert_eq!(as_count(Some(f64::INFINITY)), None);
        assert_eq!(as_count(None), None);
    }

    #[test]
    fn phases() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert_eq!(t.phase(t0), Phase::Menu);

        t.on_controller(t0, ctl(1.0));
        t.on_player(t0);
        assert_eq!(t.phase(t0), Phase::Run);

        t.on_controller(
            t0,
            Controller {
                pause: true,
                gameplay: false,
                ..ctl(1.0)
            },
        );
        assert_eq!(t.phase(t0), Phase::Paused);

        // Upgrade screen: not paused, not gameplay, ship not stepping.
        let later = t0 + Duration::from_secs(5);
        t.on_controller(
            later,
            Controller {
                gameplay: false,
                ..ctl(1.0)
            },
        );
        assert_eq!(t.phase(later), Phase::Run);

        t.on_controller(
            later,
            Controller {
                game_over: true,
                ..ctl(1.0)
            },
        );
        assert_eq!(t.phase(later), Phase::GameOver);

        // Controller gone for a while: menu.
        assert_eq!(t.phase(later + Duration::from_secs(10)), Phase::Menu);
        t.on_menu(later);
        assert_eq!(t.phase(later), Phase::Menu);
    }

    #[test]
    fn bosses_come_and_go() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.on_controller(t0, ctl(1.0));
        t.on_player(t0);
        assert_eq!(t.boss(t0), None);

        t.on_boss_created(t0, 2);
        assert_eq!(t.boss(t0), Some("Dreadnought"));
        t.on_boss_created(t0, 6);
        assert_eq!(t.boss(t0), Some("Seraph")); // most recent wins
        t.on_boss_destroyed(6);
        assert_eq!(t.boss(t0), Some("Dreadnought"));
        t.on_boss_destroyed(2);
        assert_eq!(t.boss(t0), None);
        t.on_boss_destroyed(2); // extra destroy is harmless
        t.on_boss_destroyed(99);
        assert_eq!(t.boss(t0), None);
    }

    #[test]
    fn star_eater_uses_heartbeat() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.on_controller(t0, ctl(1.0));
        t.on_player(t0);
        t.on_boss_created(t0, STAR_EATER);
        assert_eq!(t.boss(t0), Some("Star Eater"));

        let later = t0 + Duration::from_secs(5);
        t.on_controller(later, ctl(1.0));
        t.on_player(later);
        assert_eq!(t.boss(later), None, "controller stopped stepping");
        t.on_star_eater_heartbeat(later);
        assert_eq!(t.boss(later), Some("Star Eater"));
    }

    #[test]
    fn new_run_and_menu_clear_bosses() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        assert!(t.on_controller(t0, ctl(1.0)));
        assert!(!t.on_controller(t0, ctl(1.0)));
        t.on_player(t0);
        t.on_boss_created(t0, 0);
        assert!(t.on_controller(t0, ctl(2.0)));
        assert_eq!(t.boss(t0), None);

        t.on_boss_created(t0, 0);
        t.on_menu(t0);
        assert_eq!(t.boss(t0), None);
    }

    #[test]
    fn snapshot_hides_run_values_in_menu() {
        let t0 = Instant::now();
        let mut t = Tracker {
            globals: Globals {
                level: Some(3),
                wave: Some(4),
                score: Some(5),
            },
            ..Tracker::default()
        };
        assert_eq!(t.snapshot(t0).level, None);
        t.on_controller(t0, ctl(1.0));
        t.on_player(t0);
        let s = t.snapshot(t0);
        assert_eq!(
            (s.level, s.wave, s.score, s.run_secs),
            (Some(3), Some(4), Some(5), Some(60))
        );
    }
}
