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

//! Pure presence logic: game snapshot → Discord activity text/JSON.
//!
//! Templates use `{level}`, `{wave}`, `{score}`, `{time}`, `{boss}`, `{hull}`.
//! A `|` splits a template into optional segments: a segment whose placeholders
//! have no value right now is dropped, and the rest are joined with " · ".
//! Inside a segment, `~` separates alternatives: the first one with all its
//! values is used. So `"vs {boss} ~ {hull} | Score {score}"` shows
//! `vs Seraph · Score 12,345` during a boss fight and
//! `Engineer · Score 12,345` otherwise.

#![forbid(unsafe_code)]

use crate::ipc::json_str;

/// Coarse game state that selects the templates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Main menu / loadout: no run in progress.
    Menu,
    /// A run is in progress (including the upgrade screen).
    Run,
    /// A run is in progress but paused.
    Paused,
    /// The ship was destroyed; the end-of-run score screen.
    GameOver,
}

/// Values read from the game for one update.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    pub level: Option<i64>,
    pub wave: Option<i64>,
    pub score: Option<i64>,
    /// Seconds of play in the current run, if known (pauses excluded only
    /// when `show_paused` is on).
    pub run_secs: Option<u64>,
    /// Display name of the boss being fought, if any.
    pub boss: Option<&'static str>,
    /// Display name of the chosen hull, if known.
    pub hull: Option<&'static str>,
}

/// Which values are shown (`show_*` config switches).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one switch per field, mirrors the config
pub struct Toggles {
    pub level: bool,
    pub wave: bool,
    pub score: bool,
    pub time: bool,
    pub boss: bool,
    pub hull: bool,
}

impl Snapshot {
    /// Drop the values switched off in the config.
    #[must_use]
    pub fn masked(mut self, t: Toggles) -> Self {
        if !t.level {
            self.level = None;
        }
        if !t.wave {
            self.wave = None;
        }
        if !t.score {
            self.score = None;
        }
        if !t.time {
            self.run_secs = None;
        }
        if !t.boss {
            self.boss = None;
        }
        if !t.hull {
            self.hull = None;
        }
        self
    }
}

/// The activity Discord should show. `None` fields are omitted.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Activity {
    pub details: Option<String>,
    pub state: Option<String>,
    /// Unix seconds; Discord renders "mm:ss elapsed" counting from it.
    pub start: Option<u64>,
    pub large_image: Option<String>,
    pub large_text: Option<String>,
}

/// Discord accepts 2..=128 characters for text fields.
pub const MIN_TEXT: usize = 2;
pub const MAX_TEXT: usize = 128;
/// Joins the optional segments of a template.
pub const SEGMENT_JOIN: &str = " · ";

/// Trim and clamp a field to Discord's limits (`None` if too short).
#[must_use]
pub fn clamp_text(s: &str) -> Option<String> {
    let s = s.trim();
    let n = s.chars().count();
    if n < MIN_TEXT {
        return None;
    }
    if n <= MAX_TEXT {
        return Some(s.to_owned());
    }
    let mut out: String = s.chars().take(MAX_TEXT - 1).collect();
    out.push('…');
    Some(out)
}

/// `1234567` → `1,234,567` (with `sep`; empty `sep` = no grouping).
#[must_use]
pub fn group_digits(n: i64, sep: &str) -> String {
    let digits = n.unsigned_abs().to_string();
    let sign = if n < 0 { "-" } else { "" };
    if sep.is_empty() {
        return format!("{sign}{digits}");
    }
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push_str(sep);
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// `75` → `1:15`, `3725` → `1:02:05`.
#[must_use]
pub fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// One segment with every placeholder filled, or `None` if a value is missing.
fn render_segment(segment: &str, snap: &Snapshot, sep: &str) -> Option<String> {
    let fields: [(&str, Option<String>); 6] = [
        ("{level}", snap.level.map(|v| v.to_string())),
        ("{wave}", snap.wave.map(|v| v.to_string())),
        ("{score}", snap.score.map(|v| group_digits(v, sep))),
        ("{time}", snap.run_secs.map(clock)),
        ("{boss}", snap.boss.map(str::to_owned)),
        ("{hull}", snap.hull.map(str::to_owned)),
    ];
    let mut out = segment.trim().to_owned();
    for (key, value) in fields {
        if out.contains(key) {
            out = out.replace(key, &value?);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Render a template (see module docs). `None` if every segment was dropped.
#[must_use]
pub fn render(template: &str, snap: &Snapshot, thousands_sep: &str) -> Option<String> {
    let parts: Vec<String> = template
        .split('|')
        .filter_map(|seg| {
            seg.split('~')
                .find_map(|alt| render_segment(alt, snap, thousands_sep))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(SEGMENT_JOIN))
}

/// Text templates and switches (the presentation part of the config).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct Templates {
    pub run_details: String,
    pub run_state: String,
    pub paused_details: String,
    pub paused_state: String,
    pub game_over_details: String,
    pub game_over_state: String,
    pub menu_details: String,
    pub menu_state: String,
    pub show_in_menu: bool,
    pub show_elapsed: bool,
    pub large_image: String,
    pub large_text: String,
    pub thousands_separator: String,
}

/// Shown when a run template renders to nothing (e.g. all values switched off).
pub const RUN_FALLBACK: &str = "In a run";

/// Build the activity for `snap`. `run_start` is the Unix time the elapsed
/// counter should start at (see [`ElapsedClock`]). `None` = clear the presence.
#[must_use]
pub fn activity(snap: &Snapshot, t: &Templates, run_start: Option<u64>) -> Option<Activity> {
    let sep = t.thousands_separator.as_str();
    let field = |tpl: &str| render(tpl, snap, sep).as_deref().and_then(clamp_text);
    let (details, state) = match snap.phase {
        Phase::Menu if !t.show_in_menu => return None,
        Phase::Menu => (&t.menu_details, &t.menu_state),
        Phase::Run => (&t.run_details, &t.run_state),
        Phase::Paused => (&t.paused_details, &t.paused_state),
        Phase::GameOver => (&t.game_over_details, &t.game_over_state),
    };
    let mut a = Activity {
        details: field(details),
        state: field(state),
        large_image: clamp_text(&t.large_image),
        large_text: clamp_text(&t.large_text),
        ..Activity::default()
    };
    if snap.phase != Phase::Menu && a.details.is_none() && a.state.is_none() {
        a.details = Some(RUN_FALLBACK.to_owned());
    }
    // Discord can't freeze its counter, so it only runs during live play.
    if t.show_elapsed && snap.phase == Phase::Run {
        a.start = run_start;
    }
    Some(a)
}

/// The activity as a Discord JSON object.
#[must_use]
pub fn activity_json(a: &Activity) -> String {
    let mut parts = Vec::new();
    if let Some(d) = &a.details {
        parts.push(format!(r#""details":{}"#, json_str(d)));
    }
    if let Some(s) = &a.state {
        parts.push(format!(r#""state":{}"#, json_str(s)));
    }
    if let Some(start) = a.start {
        parts.push(format!(r#""timestamps":{{"start":{start}}}"#));
    }
    if let Some(img) = &a.large_image {
        let text = a
            .large_text
            .as_deref()
            .map(|t| format!(r#","large_text":{}"#, json_str(t)))
            .unwrap_or_default();
        parts.push(format!(
            r#""assets":{{"large_image":{}{text}}}"#,
            json_str(img)
        ));
    }
    format!("{{{}}}", parts.join(","))
}

/// Keeps Discord's elapsed counter stable: the start time only moves when the
/// run clock and the stored start disagree by more than [`Self::TOLERANCE`] seconds (after
/// a pause, a loaded save, or a new run), so updates don't make it jitter.
#[derive(Debug, Default)]
pub struct ElapsedClock {
    start: Option<u64>,
}

impl ElapsedClock {
    pub const TOLERANCE: u64 = 3;

    /// Start time for a run that has lasted `run_secs` at Unix time `now`.
    pub fn start_for(&mut self, now: u64, run_secs: u64) -> u64 {
        let wanted = now.saturating_sub(run_secs);
        match self.start {
            Some(s) if s.abs_diff(wanted) <= Self::TOLERANCE => s,
            _ => {
                self.start = Some(wanted);
                wanted
            }
        }
    }

    pub fn reset(&mut self) {
        self.start = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(phase: Phase) -> Snapshot {
        Snapshot {
            phase,
            level: Some(12),
            wave: Some(23),
            score: Some(1_234_567),
            run_secs: Some(754),
            boss: None,
            hull: None,
        }
    }

    fn templates() -> Templates {
        Templates {
            run_details: "Wave {wave} | Level {level}".into(),
            run_state: "vs {boss} | Score {score}".into(),
            paused_details: "Paused | Wave {wave}".into(),
            paused_state: "Score {score}".into(),
            game_over_details: "Run over | Wave {wave}".into(),
            game_over_state: "Final score {score}".into(),
            menu_details: "In the menus".into(),
            menu_state: String::new(),
            show_in_menu: true,
            show_elapsed: true,
            large_image: "logo".into(),
            large_text: "Nova Drift".into(),
            thousands_separator: ",".into(),
        }
    }

    const ALL: Toggles = Toggles {
        level: true,
        wave: true,
        score: true,
        time: true,
        boss: true,
        hull: true,
    };

    #[test]
    fn number_formatting() {
        assert_eq!(group_digits(0, ","), "0");
        assert_eq!(group_digits(999, ","), "999");
        assert_eq!(group_digits(1000, ","), "1,000");
        assert_eq!(group_digits(1_234_567, " "), "1 234 567");
        assert_eq!(group_digits(-4321, ","), "-4,321");
        assert_eq!(group_digits(1_234_567, ""), "1234567");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(3725), "1:02:05");
    }

    #[test]
    fn placeholders_and_segments() {
        let s = snap(Phase::Run);
        assert_eq!(
            render("W{wave} L{level} {score} {time}", &s, ",").as_deref(),
            Some("W23 L12 1,234,567 12:34")
        );
        // Missing boss: that segment is dropped.
        assert_eq!(
            render("vs {boss} | Score {score}", &s, ",").as_deref(),
            Some("Score 1,234,567")
        );
        let boss = Snapshot {
            boss: Some("Star Eater"),
            ..s
        };
        assert_eq!(
            render("vs {boss} | Score {score}", &boss, ",").as_deref(),
            Some("vs Star Eater · Score 1,234,567")
        );
        let nothing = Snapshot {
            wave: None,
            level: None,
            ..s
        };
        assert_eq!(render("Wave {wave} | Level {level}", &nothing, ","), None);
        assert_eq!(render(" | ", &s, ","), None);
        assert_eq!(
            render("Paused | Wave {wave}", &nothing, ",").as_deref(),
            Some("Paused")
        );
    }

    #[test]
    fn alternatives_pick_first_with_values() {
        let s = snap(Phase::Run);
        let tpl = "vs {boss} ~ {hull} | Score {score}";
        // Neither boss nor hull: the whole segment is dropped.
        assert_eq!(render(tpl, &s, ",").as_deref(), Some("Score 1,234,567"));
        let hull = Snapshot {
            hull: Some("Engineer"),
            ..s
        };
        assert_eq!(
            render(tpl, &hull, ",").as_deref(),
            Some("Engineer · Score 1,234,567")
        );
        let both = Snapshot {
            boss: Some("Seraph"),
            ..hull
        };
        assert_eq!(
            render(tpl, &both, ",").as_deref(),
            Some("vs Seraph · Score 1,234,567"),
            "a boss fight wins over the hull"
        );
        let masked = both.masked(Toggles { boss: false, ..ALL });
        assert_eq!(
            render(tpl, &masked, ",").as_deref(),
            Some("Engineer · Score 1,234,567")
        );
    }

    #[test]
    fn toggles_mask_values() {
        let s = Snapshot {
            boss: Some("Seraph"),
            ..snap(Phase::Run)
        };
        assert_eq!(s.masked(ALL), s);
        let m = s.masked(Toggles {
            score: false,
            boss: false,
            ..ALL
        });
        assert_eq!((m.score, m.boss, m.level), (None, None, Some(12)));
    }

    #[test]
    fn text_limits() {
        assert_eq!(clamp_text(" x "), None);
        assert_eq!(clamp_text("ok").as_deref(), Some("ok"));
        let c = clamp_text(&"a".repeat(200)).unwrap();
        assert_eq!(c.chars().count(), MAX_TEXT);
        assert!(c.ends_with('…'));
    }

    #[test]
    fn run_activity() {
        let a = activity(&snap(Phase::Run), &templates(), Some(1000)).unwrap();
        assert_eq!(a.details.as_deref(), Some("Wave 23 · Level 12"));
        assert_eq!(a.state.as_deref(), Some("Score 1,234,567"));
        assert_eq!(a.start, Some(1000));
        assert_eq!(
            activity_json(&a),
            r#"{"details":"Wave 23 · Level 12","state":"Score 1,234,567","timestamps":{"start":1000},"assets":{"large_image":"logo","large_text":"Nova Drift"}}"#
        );
    }

    #[test]
    fn run_without_values_falls_back() {
        let s = Snapshot {
            level: None,
            wave: None,
            score: None,
            ..snap(Phase::Run)
        };
        let a = activity(&s, &templates(), None).unwrap();
        assert_eq!(a.details.as_deref(), Some(RUN_FALLBACK));
        assert_eq!(a.state, None);
    }

    #[test]
    fn other_phases() {
        let p = activity(&snap(Phase::Paused), &templates(), Some(1000)).unwrap();
        assert_eq!(p.details.as_deref(), Some("Paused · Wave 23"));
        assert_eq!(p.start, None);

        let g = activity(&snap(Phase::GameOver), &templates(), Some(1000)).unwrap();
        assert_eq!(g.state.as_deref(), Some("Final score 1,234,567"));
        assert_eq!(g.start, None);

        let m = activity(&snap(Phase::Menu), &templates(), None).unwrap();
        assert_eq!(m.details.as_deref(), Some("In the menus"));
        assert_eq!(m.state, None);

        let hidden = Templates {
            show_in_menu: false,
            ..templates()
        };
        assert_eq!(activity(&snap(Phase::Menu), &hidden, None), None);
    }

    #[test]
    fn empty_activity_json() {
        assert_eq!(activity_json(&Activity::default()), "{}");
    }

    #[test]
    fn elapsed_clock_is_stable() {
        let mut c = ElapsedClock::default();
        assert_eq!(c.start_for(1_000, 100), 900);
        assert_eq!(c.start_for(1_010, 109), 900); // 1 s drift: keep
        assert_eq!(c.start_for(1_100, 100), 1_000); // paused 100 s: move
        c.reset();
        assert_eq!(c.start_for(5_000, 0), 5_000);
    }
}
