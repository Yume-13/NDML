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

//! # Magnet Range
//!
//! Multiplies the distance from which XP orbs (`oBounty*`) and power-ups are
//! pulled toward the ship.
//!
//! ## Game formula (Nova Drift 1.2.38.4)
//! Every collectable runs `powerUpInit(300)` on creation and `powerupAttract()`
//! every step. With `A = oPowers.powerupAttraction` (the Magnetism stat):
//!
//! ```gml
//! // powerUpInit(300)
//! mag = 300 * A;  magAdd = 0.01 * mag;  magCurrent = mag;      // magDist = 300 (oCollectable)
//! // powerupAttract()
//! powDist = distance_to_object(oPlayer);
//! if (powDist > magDist * A) magCurrent = mag;                  // out of range: reset
//! else {
//!     motion_add(point_direction(x, y, oPlayer.x, oPlayer.y),
//!                0.15 + 0.001 * (magCurrent - powDist));        // friction is 0.15
//!     magCurrent += magAdd;
//! }
//! ```
//!
//! Friction cancels the 0.15, so an orb is only pulled once `magCurrent`
//! exceeds its distance. `magDist` merely gates that; **range and pull are one
//! coupled field**. Scaling `magDist` alone lets distant orbs pass the gate
//! while `magCurrent` is still too small to move them.
//!
//! ## Mechanism
//! Wrap `gml_Script_powerupAttract`: multiply `magDist`, `mag`, `magAdd` and
//! `magCurrent` by the factor before it runs and divide afterwards. Everything
//! inside is linear in these, so the state stored between steps stays exactly
//! vanilla while each step behaves as if the field were `factor` times larger.
//! The effect matches a higher Magnetism stat, limited to objects that run
//! `powerupAttract`.

#![forbid(unsafe_code)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ndml_sdk::{Error, ModContext, Variable};
use serde::Deserialize;

/// The hooked script (shared by every orb and power-up).
pub const ATTRACT_SCRIPT: &str = "gml_Script_powerupAttract";
/// Per-instance variables making up the attraction field, all linear in the
/// Magnetism stat: gate distance, base pull, pull growth, current pull.
pub const FIELD_VARS: [&str; 4] = ["magDist", "mag", "magAdd", "magCurrent"];
/// Upper bound to keep a typo from attracting from across the galaxy.
pub const MAX_MULTIPLIER: f64 = 50.0;

/// `ndml/config/ndmod_magnet_range.toml`; shipped defaults in `config.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Range multiplier. 1.0 = vanilla, 3.0 = three times the pickup distance.
    pub range_multiplier: f64,
}

impl Config {
    /// The multiplier clamped to a sane range (`None` if it is a no-op/invalid).
    #[must_use]
    pub fn effective_multiplier(&self) -> Option<f64> {
        let m = self.range_multiplier;
        if !m.is_finite() || m <= 0.0 {
            return None;
        }
        let m = m.min(MAX_MULTIPLIER);
        ((m - 1.0).abs() > f64::EPSILON).then_some(m)
    }
}

/// Value to store back after the game ran on a scaled variable: the exact
/// original if the game didn't touch it (no float drift), otherwise the game's
/// result divided back down.
#[must_use]
pub fn restored(original: f64, scaled: f64, after: f64, mult: f64) -> f64 {
    if after.to_bits() == scaled.to_bits() {
        original
    } else {
        after / mult
    }
}

/// `(original, scaled)` per field variable, `None` where the instance lacks it.
type Saved = [Option<(f64, f64)>; FIELD_VARS.len()];

/// Outcome tally of hook calls, logged once a minute at debug level so calls
/// that could not be scaled are visible in the log.
static SCALED: AtomicU64 = AtomicU64::new(0);
static PARTIAL: AtomicU64 = AtomicU64::new(0);
static NO_INSTANCE: AtomicU64 = AtomicU64::new(0);
static TALLY_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
const TALLY_PERIOD: Duration = Duration::from_secs(60);

fn tally(saved: &Saved, had_instance: bool) {
    let counter = if !had_instance {
        &NO_INSTANCE
    } else if saved.iter().all(Option::is_some) {
        &SCALED
    } else {
        &PARTIAL
    };
    counter.fetch_add(1, Ordering::Relaxed);
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let Ok(mut since) = TALLY_SINCE.try_lock() else {
        return;
    };
    let now = Instant::now();
    match *since {
        None => *since = Some(now),
        Some(t) if now.duration_since(t) >= TALLY_PERIOD => {
            *since = Some(now);
            drop(since);
            log::debug!(
                "last {}s: {} calls scaled, {} partly (some field var missing), {} without instance",
                now.duration_since(t).as_secs(),
                SCALED.swap(0, Ordering::Relaxed),
                PARTIAL.swap(0, Ordering::Relaxed),
                NO_INSTANCE.swap(0, Ordering::Relaxed),
            );
        }
        Some(_) => {}
    }
}

fn init(ctx: &ModContext) -> Result<(), Error> {
    let config: Config = ctx.load_config()?;
    let Some(mult) = config.effective_multiplier() else {
        log::warn!(
            "range_multiplier = {} is vanilla or invalid; mod inactive",
            config.range_multiplier
        );
        return Ok(());
    };
    if mult < config.range_multiplier {
        log::warn!("range_multiplier clamped to {MAX_MULTIPLIER}");
    }
    let mut vars: Vec<Variable> = Vec::with_capacity(FIELD_VARS.len());
    for name in FIELD_VARS {
        vars.push(ctx.variable(name)?);
    }
    let (vars_before, vars_after) = (vars.clone(), vars);

    ctx.hook(
        ATTRACT_SCRIPT,
        move |call| -> Saved {
            static FIRST: AtomicBool = AtomicBool::new(true);
            let mut saved: Saved = [None; FIELD_VARS.len()];
            let Some(me) = call.instance() else {
                tally(&saved, false);
                return saved;
            };
            for (slot, var) in saved.iter_mut().zip(&vars_before) {
                if let Some(original) = me.get_f64(*var) {
                    let scaled = original * mult;
                    if me.set_f64(*var, scaled) {
                        *slot = Some((original, scaled));
                    }
                }
            }
            if FIRST.swap(false, Ordering::Relaxed) {
                log::info!("active on first collectable (original, scaled): {saved:?}");
            }
            tally(&saved, true);
            saved
        },
        move |call, saved| {
            let Some(me) = call.instance() else { return };
            for (entry, var) in saved.iter().zip(&vars_after) {
                let Some((original, scaled)) = *entry else {
                    continue;
                };
                let after = me.get_f64(*var).unwrap_or(scaled);
                if !me.set_f64(*var, restored(original, scaled, after, mult)) {
                    log::warn!("could not restore an attraction variable");
                }
            }
        },
    )?;
    log::info!("XP orb / power-up attraction range x{mult}");
    Ok(())
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_parse() {
        let cfg: Config = ndml_sdk::config::parse_defaults(NDML_DEFAULT_CONFIG).unwrap();
        assert!((cfg.range_multiplier - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn multiplier_validation() {
        let m = |range_multiplier| Config { range_multiplier }.effective_multiplier();
        assert_eq!(m(2.0), Some(2.0));
        assert_eq!(m(1.0), None);
        assert_eq!(m(0.0), None);
        assert_eq!(m(-3.0), None);
        assert_eq!(m(f64::NAN), None);
        assert_eq!(m(1000.0), Some(MAX_MULTIPLIER));
        assert_eq!(m(0.5), Some(0.5));
    }

    /// Simulate the game's attraction step on (magDist, mag, magAdd, magCurrent)
    /// and return (pull acceleration, new magCurrent).
    fn game_step(f: [f64; 4], attraction: f64, dist: f64) -> (f64, f64) {
        let [mag_dist, mag, mag_add, mag_current] = f;
        if dist > mag_dist * attraction {
            (0.0, mag)
        } else {
            (0.001 * (mag_current - dist), mag_current + mag_add)
        }
    }

    #[test]
    fn scaled_field_pulls_at_triple_range_and_restores_exactly() {
        let (a, k) = (1.0, 3.0);
        let field = [300.0, 300.0 * a, 3.0 * a, 300.0 * a]; // straight after powerUpInit(300)
        let dist = 800.0; // beyond vanilla range (300), inside 3x (900)

        // Vanilla: out of range, no pull.
        assert!(game_step(field, a, dist).0.abs() < f64::EPSILON);

        // Modded step: scale, run, restore.
        let scaled = field.map(|v| v * k);
        let (pull, new_current) = game_step(scaled, a, dist);
        assert!(pull > 0.0, "orb at 800 must be pulled with a 3x field");
        let stored = [
            restored(field[0], scaled[0], scaled[0], k),
            restored(field[1], scaled[1], scaled[1], k),
            restored(field[2], scaled[2], scaled[2], k),
            restored(field[3], scaled[3], new_current, k),
        ];
        // Untouched vars come back bit-exact; magCurrent advanced by one vanilla magAdd.
        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&stored[..3]), bits(&field[..3]));
        assert!((stored[3] - (field[3] + field[2])).abs() < 1e-9);
    }
}
