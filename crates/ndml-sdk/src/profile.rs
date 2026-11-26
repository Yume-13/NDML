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

//! Frame profile (diagnostics, off unless a mod turns it on): the time a
//! mod spends inside each of its hooks (before and after parts apart, with
//! call counts) and inside named spans of its own code, per frame, so a long
//! frame can be split into the mod's parts and the remainder that is not the
//! mod's.
//!
//! Every hook registered through [`crate::ModContext`] is timed when
//! [`enable`] is on (one atomic load per call when off). Hooks that run
//! inside another hook of this mod (a script the mod calls that is hooked
//! too) are listed but not added twice to the mod's total. The mod marks its
//! frame boundary with [`frame`], which returns the frame that just ended.
//!
//! Apart from it, [`time_sides`] keeps only two sums, no list: the mod's
//! time in its step-side and its draw-side hooks ([`take_sides`]), for a
//! line a mod can write while the game runs slow. Off, it costs nothing
//! more than the same atomic load.

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// [`FULL`] and [`SIDES`]: what is timed (one load per hook call).
static MODE: AtomicU8 = AtomicU8::new(0);
/// The frame profile ([`enable`]).
const FULL: u8 = 1;
/// The two sums ([`time_sides`]).
const SIDES: u8 = 2;
/// The mod's top-level hook time: step side, draw side (ns).
static SIDE_NS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static TOP_NS: AtomicU64 = AtomicU64::new(0);
static SLOTS: Mutex<Vec<Arc<Slot>>> = Mutex::new(Vec::new());
static SPANS: Mutex<Vec<&'static Span>> = Mutex::new(Vec::new());
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

fn set_mode(bit: u8, on: bool) {
    if on {
        MODE.fetch_or(bit, Ordering::Relaxed);
    } else {
        MODE.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// Turn the profile on or off (diagnostics).
pub fn enable(on: bool) {
    set_mode(FULL, on);
}

/// Whether the profile is on.
#[must_use]
pub fn enabled() -> bool {
    MODE.load(Ordering::Relaxed) & FULL != 0
}

/// Sum the mod's time in its step-side and draw-side hooks (top level only:
/// a hook inside another is in that one's time), or stop. Read and reset
/// with [`take_sides`].
pub fn time_sides(on: bool) {
    set_mode(SIDES, on);
}

/// The mod's hook time since the last call: (step side, draw side). A hook
/// is draw-side when it hooks a draw event (`..._Draw_N`, an observer of
/// event kind 8) or a `draw_` builtin; every other hook is step-side.
#[must_use]
pub fn take_sides() -> (Duration, Duration) {
    let take = |a: &AtomicU64| Duration::from_nanos(a.swap(0, Ordering::Relaxed));
    (take(&SIDE_NS[0]), take(&SIDE_NS[1]))
}

/// Whether hooked function `function` (as registered) runs on the draw side.
fn draw_side(function: &str) -> bool {
    function.contains("_Draw_")
        || function.starts_with("event 8/")
        || function.starts_with("builtin:draw_")
}

/// One registered hook's time this frame.
#[derive(Debug)]
pub(crate) struct Slot {
    label: String,
    /// Its time is draw-side ([`take_sides`]).
    draw: bool,
    ns: [AtomicU64; 2],
    calls: [AtomicU32; 2],
}

/// A hook as registered: the hooked function and where in the mod.
pub(crate) fn register(function: &str, at: &Location<'_>) -> Arc<Slot> {
    let file = at.file().rsplit(['/', '\\']).next().unwrap_or(at.file());
    let short = function
        .strip_prefix("gml_Object_")
        .or_else(|| function.strip_prefix("gml_Script_"))
        .unwrap_or(function);
    let slot = Arc::new(Slot {
        label: format!("{short} ({file}:{})", at.line()),
        draw: draw_side(function),
        ns: [AtomicU64::new(0), AtomicU64::new(0)],
        calls: [AtomicU32::new(0), AtomicU32::new(0)],
    });
    if let Ok(mut v) = SLOTS.lock() {
        v.push(Arc::clone(&slot));
    }
    slot
}

/// Run `f` as part `part` (0 before, 1 after) of hook `slot`.
pub(crate) fn timed<R>(slot: &Slot, part: usize, f: impl FnOnce() -> R) -> R {
    let mode = MODE.load(Ordering::Relaxed);
    if mode == 0 {
        return f();
    }
    let depth = DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let t0 = Instant::now();
    let r = f();
    let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
    DEPTH.with(|d| d.set(depth));
    if mode & SIDES != 0 && depth == 0 {
        SIDE_NS[usize::from(slot.draw)].fetch_add(ns, Ordering::Relaxed);
    }
    if mode & FULL == 0 {
        return r;
    }
    slot.ns[part].fetch_add(ns, Ordering::Relaxed);
    slot.calls[part].fetch_add(1, Ordering::Relaxed);
    if depth == 0 {
        TOP_NS.fetch_add(ns, Ordering::Relaxed);
    }
    r
}

/// A named part of a mod's own code, timed with [`span`]:
/// `static WORK: Span = Span::new("work");`.
#[derive(Debug)]
pub struct Span {
    name: &'static str,
    ns: AtomicU64,
    calls: AtomicU32,
    listed: AtomicBool,
}

impl Span {
    /// A span named `name`.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            ns: AtomicU64::new(0),
            calls: AtomicU32::new(0),
            listed: AtomicBool::new(false),
        }
    }
}

/// Run `f` inside span `s` (only timed while the profile is on).
pub fn span<R>(s: &'static Span, f: impl FnOnce() -> R) -> R {
    let _inside = enter(s);
    f()
}

/// Span `s` as a guard: timed from here until the guard drops (only while
/// the profile is on), for code that is not one closure.
pub fn enter(s: &'static Span) -> Entered {
    if !enabled() {
        return Entered(None);
    }
    if !s.listed.swap(true, Ordering::Relaxed)
        && let Ok(mut v) = SPANS.lock()
    {
        v.push(s);
    }
    Entered(Some((s, Instant::now())))
}

/// A span entered with [`enter`]; its time counts when it drops.
#[derive(Debug)]
#[must_use = "the span ends when this drops"]
pub struct Entered(Option<(&'static Span, Instant)>);

impl Drop for Entered {
    fn drop(&mut self) {
        if let Some((s, t0)) = self.0.take() {
            let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
            s.ns.fetch_add(ns, Ordering::Relaxed);
            s.calls.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A frame as the profile saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    /// From the previous boundary to this one.
    pub wall: Duration,
    /// Time inside the mod's hooks, not counting a hook inside another.
    pub mod_time: Duration,
    /// (label, before, before calls, after, after calls), largest first.
    pub hooks: Vec<(String, Duration, u32, Duration, u32)>,
    /// (span, time, calls), largest first.
    pub spans: Vec<(&'static str, Duration, u32)>,
}

impl Frame {
    /// The wall time not spent in the mod's hooks.
    #[must_use]
    pub fn remainder(&self) -> Duration {
        self.wall.saturating_sub(self.mod_time)
    }

    /// One line: wall, the mod's sum, the remainder, the spans and the
    /// `top` largest hooks.
    #[must_use]
    pub fn line(&self, top: usize) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut s = format!(
            "wall {:.1} ms; the mod's hooks {:.2} ms; not the mod's {:.1} ms",
            ms(self.wall),
            ms(self.mod_time),
            ms(self.remainder())
        );
        if !self.spans.is_empty() {
            s.push_str("; spans:");
            for (n, d, c) in &self.spans {
                let _ = write!(s, " {n} {:.2} ms x{c}", ms(*d));
            }
        }
        if !self.hooks.is_empty() {
            s.push_str("; hooks:");
            for (l, b, bc, a, ac) in self.hooks.iter().take(top) {
                let _ = write!(
                    s,
                    " {l} before {:.2} ms x{bc} after {:.2} ms x{ac};",
                    ms(*b),
                    ms(*a)
                );
            }
        }
        s
    }
}

/// The frame boundary (call once per frame from the same place): the frame
/// that just ended, and the counters reset; `None` on the first call or
/// while the profile is off.
pub fn frame(now: Instant) -> Option<Frame> {
    if !enabled() {
        return None;
    }
    let last = LAST.lock().ok()?.replace(now)?;
    let take = |a: &AtomicU64| Duration::from_nanos(a.swap(0, Ordering::Relaxed));
    let mut f = Frame {
        wall: now.saturating_duration_since(last),
        mod_time: take(&TOP_NS),
        ..Frame::default()
    };
    if let Ok(v) = SLOTS.lock() {
        for s in v.iter() {
            let (b, a) = (take(&s.ns[0]), take(&s.ns[1]));
            let (bc, ac) = (
                s.calls[0].swap(0, Ordering::Relaxed),
                s.calls[1].swap(0, Ordering::Relaxed),
            );
            if bc + ac > 0 {
                f.hooks.push((s.label.clone(), b, bc, a, ac));
            }
        }
    }
    if let Ok(v) = SPANS.lock() {
        for s in v.iter() {
            let c = s.calls.swap(0, Ordering::Relaxed);
            let d = take(&s.ns);
            if c > 0 {
                f.spans.push((s.name, d, c));
            }
        }
    }
    f.hooks.sort_by_key(|h| std::cmp::Reverse(h.1 + h.3));
    f.spans.sort_by_key(|s| std::cmp::Reverse(s.1));
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Held by each test: the profile's state is the process's.
    static TURN: Mutex<()> = Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        TURN.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hook and span time per frame: nested hooks counted once, the counts,
    /// the order and the reset.
    #[test]
    fn hooks_and_spans_split_a_frame() {
        static WORK: Span = Span::new("work");
        static GUARDED: Span = Span::new("guarded");
        let _turn = turn();
        enable(true);
        let t0 = Instant::now();
        let _ = frame(t0);
        let outer = register("gml_Script_outer", Location::caller());
        let inner = register("gml_Script_inner", Location::caller());
        timed(&outer, 0, || {
            span(&WORK, || std::thread::sleep(Duration::from_millis(3)));
            drop(enter(&GUARDED));
            timed(&inner, 1, || std::thread::sleep(Duration::from_millis(2)));
        });
        let f = frame(Instant::now()).unwrap();
        assert!(f.mod_time >= Duration::from_millis(5), "{:?}", f.mod_time);
        // Both read the same monotonic clock, the wall around the hooks.
        assert!(f.wall >= f.mod_time);
        assert_eq!(f.hooks.len(), 2);
        assert!(f.hooks[0].0.starts_with("outer (profile.rs:"));
        assert!(f.hooks[1].0.starts_with("inner (profile.rs:"));
        assert_eq!((f.hooks[0].2, f.hooks[1].4), (1, 1));
        // The inner hook is listed, not added twice: the sum is the outer
        // hook's own time, which holds the inner one's.
        assert_eq!(f.mod_time, f.hooks[0].1);
        assert!(f.hooks[0].1 >= f.hooks[1].3 + Duration::from_millis(3));
        assert_eq!(f.spans[0].0, "work");
        assert_eq!(
            (f.spans[1].0, f.spans[1].2),
            ("guarded", 1),
            "a guard counts as it drops"
        );
        assert!(f.line(5).contains("not the mod's"));
        // Reset for the next frame.
        let g = frame(Instant::now()).unwrap();
        assert!(g.hooks.is_empty() && g.mod_time == Duration::ZERO);
        enable(false);
        assert!(frame(Instant::now()).is_none());
    }

    /// The step-side and draw-side sums, with the profile off and on:
    /// nested hooks counted once.
    #[test]
    fn the_two_sides_are_summed_without_the_profile() {
        let _turn = turn();
        let step = register("gml_Object_oWidget_Step_0", Location::caller());
        let draw = register("gml_Object_oThing_Draw_64", Location::caller());
        let observer = register("event 8/0", Location::caller());
        let nested = register("gml_Script_inner", Location::caller());
        assert!(!step.draw && draw.draw && observer.draw && !nested.draw);
        let _ = take_sides();
        timed(&step, 1, || std::thread::sleep(Duration::from_millis(2)));
        assert_eq!(
            take_sides(),
            (Duration::ZERO, Duration::ZERO),
            "off: nothing"
        );
        time_sides(true);
        assert!(!enabled(), "the profile stays off");
        timed(&step, 1, || {
            std::thread::sleep(Duration::from_millis(2));
            // A hook inside another is in that one's time already.
            timed(&nested, 0, || std::thread::sleep(Duration::from_millis(1)));
        });
        timed(&draw, 0, || std::thread::sleep(Duration::from_millis(1)));
        timed(&observer, 0, || ());
        let (s, d) = take_sides();
        assert!(s >= Duration::from_millis(3), "{s:?}");
        assert!(d >= Duration::from_millis(1), "{d:?}");
        assert_eq!(take_sides(), (Duration::ZERO, Duration::ZERO), "taken");
        // A hook inside another is not added twice: with the profile on as
        // well, each side is exactly its top-level hooks' own time.
        enable(true);
        let _ = frame(Instant::now());
        timed(&step, 1, || {
            timed(&nested, 0, || std::thread::sleep(Duration::from_millis(1)));
        });
        timed(&draw, 0, || ());
        let (s, d) = take_sides();
        let f = frame(Instant::now()).unwrap();
        enable(false);
        time_sides(false);
        let own = |label: &str| {
            f.hooks
                .iter()
                .find(|h| h.0.starts_with(label))
                .map(|h| h.1 + h.3)
                .unwrap()
        };
        assert_eq!(s, own("oWidget_Step_0 ("), "the nested hook's time once");
        assert_eq!(d, own("oThing_Draw_64 ("));
        assert_eq!(s + d, f.mod_time);
    }
}
