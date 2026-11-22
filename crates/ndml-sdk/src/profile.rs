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

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static TOP_NS: AtomicU64 = AtomicU64::new(0);
static SLOTS: Mutex<Vec<Arc<Slot>>> = Mutex::new(Vec::new());
static SPANS: Mutex<Vec<&'static Span>> = Mutex::new(Vec::new());
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Turn the profile on or off (diagnostics).
pub fn enable(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the profile is on.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One registered hook's time this frame.
#[derive(Debug)]
pub(crate) struct Slot {
    label: String,
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
    if !enabled() {
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
    if !enabled() {
        return f();
    }
    if !s.listed.swap(true, Ordering::Relaxed)
        && let Ok(mut v) = SPANS.lock()
    {
        v.push(s);
    }
    let t0 = Instant::now();
    let r = f();
    let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
    s.ns.fetch_add(ns, Ordering::Relaxed);
    s.calls.fetch_add(1, Ordering::Relaxed);
    r
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

    #[test]
    fn hooks_and_spans_split_a_frame() {
        static WORK: Span = Span::new("work");
        enable(true);
        let t0 = Instant::now();
        let _ = frame(t0);
        let outer = register("gml_Script_outer", Location::caller());
        let inner = register("gml_Script_inner", Location::caller());
        timed(&outer, 0, || {
            span(&WORK, || std::thread::sleep(Duration::from_millis(3)));
            timed(&inner, 1, || std::thread::sleep(Duration::from_millis(2)));
        });
        let f = frame(Instant::now()).unwrap();
        // The inner hook is listed, not added twice.
        assert!(f.mod_time >= Duration::from_millis(5) && f.mod_time < Duration::from_millis(50));
        assert!(f.wall >= f.mod_time);
        assert_eq!(f.hooks.len(), 2);
        assert!(f.hooks[0].0.starts_with("outer (profile.rs:"));
        assert_eq!((f.hooks[0].2, f.hooks[1].4), (1, 1));
        assert_eq!(f.spans[0].0, "work");
        assert!(f.line(5).contains("not the mod's"));
        // Reset for the next frame.
        let g = frame(Instant::now()).unwrap();
        assert!(g.hooks.is_empty() && g.mod_time == Duration::ZERO);
        enable(false);
        assert!(frame(Instant::now()).is_none());
    }
}
