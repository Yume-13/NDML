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

//! GML error guard (off unless a mod asks for it).
//!
//! In this YYC runner a GML runtime error is a C++ exception: `YYError` throws a
//! `YYGMLException`. The game's own handlers show the "Code Error" dialog and
//! end the process. Two guards turn that into a contained error, both on the
//! game thread only:
//!
//! * **Event guard**: a detour on the runner's `ExecuteIt` (which runs one
//!   compiled event for an instance) runs the event inside an SEH frame when
//!   the instance is one a mod marked, or is in a mod's wider scope (what a
//!   contained event made; named objects: [`set_scope`]), or when the event
//!   runs inside a mod's guarded call. An error raised there abandons that
//!   event only, exactly like a GML `try`/`catch` around the event body, and
//!   the game continues. Errors in the game's own instances are never taken:
//!   the frame only takes an error when it is the innermost event on the
//!   stack.
//! * **Call guard**: script and builtin calls a mod makes run inside the same
//!   SEH frame; an error becomes `Status::GmlError` for the caller instead of
//!   a process abort (Rust's `catch_unwind` aborts on a foreign exception).
//!
//! This module holds the safe parts: finding what the guard needs in the
//! image (by shape, no fixed addresses), the marked-instance set, the event
//! nesting depth the filter decides on, and the queue of caught errors. The
//! SEH thunk lives in `win`, the detour and the state restore in `hooks`, the
//! mod-facing calls in `host`.

#![forbid(unsafe_code)]

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::pe::{self, Layout};
use crate::yyc::Image;

/// Event guard requested (a mod asked during init).
pub const EVENTS: u32 = 1;
/// Call guard requested.
pub const CALLS: u32 = 2;

/// What the mods asked for (`EVENTS | CALLS`).
static REQUESTED: AtomicU32 = AtomicU32::new(0);
/// The event detour is installed and the runner state to restore was found.
static EVENTS_LIVE: AtomicBool = AtomicBool::new(false);
/// The call guard can run (its runner state was found).
static CALLS_LIVE: AtomicBool = AtomicBool::new(false);

/// Boot has decided whether the guard runs.
static BOOTED: AtomicBool = AtomicBool::new(false);

pub fn mark_booted() {
    BOOTED.store(true, Ordering::Relaxed);
}

#[must_use]
pub fn booted() -> bool {
    BOOTED.load(Ordering::Relaxed)
}

/// A mod asks for `flags` (`EVENTS`, `CALLS`). The event guard needs this
/// before hooks are installed (mod init); the call guard can be asked any time.
pub fn request(flags: u32) {
    REQUESTED.fetch_or(flags & (EVENTS | CALLS), Ordering::Relaxed);
}

/// A mod turns the call guard off again (`flags` bits cleared).
pub fn release(flags: u32) {
    REQUESTED.fetch_and(!(flags & CALLS), Ordering::Relaxed);
}

#[must_use]
pub fn requested(flags: u32) -> bool {
    REQUESTED.load(Ordering::Relaxed) & flags != 0
}

pub fn set_events_live(on: bool) {
    EVENTS_LIVE.store(on, Ordering::Relaxed);
}

pub fn set_calls_live(on: bool) {
    CALLS_LIVE.store(on, Ordering::Relaxed);
}

#[must_use]
pub fn events_live() -> bool {
    EVENTS_LIVE.load(Ordering::Relaxed)
}

/// The call guard wraps mod calls now.
#[must_use]
pub fn calls_on() -> bool {
    CALLS_LIVE.load(Ordering::Relaxed) && requested(CALLS)
}

// ---- marked instances ----

// The detour asks `is_marked` for every event, so the lookup takes no lock:
// an open-addressing table of atomic slots (linear probing), rebuilt by
// `set_marked`. Both sides run on the game thread; were `set_marked`
// called from another thread, a lookup during the rebuild could answer wrong
// for that one event (it would or would not be guarded), never unsafely.
// More ids than the table holds at half load go to the locked overflow set.

/// Table slots (a power of two).
const MARK_SLOTS: usize = 4096;
/// Ids the table takes (half load keeps probes short).
const MARK_TABLE_MAX: usize = MARK_SLOTS / 2;
/// An empty slot (instance ids are positive).
const MARK_EMPTY: i32 = i32::MIN;

static MARK_TABLE: [AtomicI32; MARK_SLOTS] = [const { AtomicI32::new(MARK_EMPTY) }; MARK_SLOTS];
/// Slots filled now (the writer's own bookkeeping, to clear them next time).
static MARK_FILLED: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Ids past [`MARK_TABLE_MAX`] (rare): looked up under a lock.
static MARK_OVERFLOW: Mutex<Option<HashSet<i32>>> = Mutex::new(None);
static ANY_OVERFLOW: AtomicBool = AtomicBool::new(false);
static ANY_MARKED: AtomicBool = AtomicBool::new(false);
static MARKED_COUNT: AtomicUsize = AtomicUsize::new(0);

fn mark_slot(id: i32) -> usize {
    // Fibonacci hashing: instance ids are dense and sequential.
    (id as u32).wrapping_mul(0x9E37_79B1) as usize >> (32 - MARK_SLOTS.trailing_zeros())
}

/// Whether `id` is in the lock-free table.
fn in_table(id: i32) -> bool {
    let mut s = mark_slot(id);
    // At most half the slots are filled, so an empty one ends every probe.
    for _ in 0..MARK_SLOTS {
        match MARK_TABLE[s].load(Ordering::Relaxed) {
            MARK_EMPTY => return false,
            v if v == id => return true,
            _ => s = (s + 1) & (MARK_SLOTS - 1),
        }
    }
    false
}

/// Replace the set of instance ids mods marked.
pub fn set_marked(ids: impl IntoIterator<Item = i32>) {
    let mut filled = MARK_FILLED.lock().unwrap_or_else(PoisonError::into_inner);
    for &s in filled.iter() {
        MARK_TABLE[s].store(MARK_EMPTY, Ordering::Relaxed);
    }
    filled.clear();
    let mut overflow = HashSet::new();
    let mut count = 0;
    for id in ids {
        if id == MARK_EMPTY {
            continue;
        }
        if filled.len() >= MARK_TABLE_MAX {
            if !in_table(id) {
                count += usize::from(overflow.insert(id));
            }
            continue;
        }
        let mut s = mark_slot(id);
        loop {
            match MARK_TABLE[s].load(Ordering::Relaxed) {
                MARK_EMPTY => {
                    MARK_TABLE[s].store(id, Ordering::Relaxed);
                    filled.push(s);
                    count += 1;
                    break;
                }
                v if v == id => break,
                _ => s = (s + 1) & (MARK_SLOTS - 1),
            }
        }
    }
    ANY_OVERFLOW.store(!overflow.is_empty(), Ordering::Relaxed);
    *MARK_OVERFLOW.lock().unwrap_or_else(PoisonError::into_inner) =
        (!overflow.is_empty()).then_some(overflow);
    MARKED_COUNT.store(count, Ordering::Relaxed);
    ANY_MARKED.store(count > 0, Ordering::Relaxed);
}

/// Whether instance `id` is marked. Lock-free unless more than
/// `MARK_TABLE_MAX` instances are marked.
#[must_use]
pub fn is_marked(id: i32) -> bool {
    if !ANY_MARKED.load(Ordering::Relaxed) || id == MARK_EMPTY {
        return false;
    }
    if in_table(id) {
        return true;
    }
    ANY_OVERFLOW.load(Ordering::Relaxed)
        && MARK_OVERFLOW
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|s| s.contains(&id))
}

/// How many instances are marked now.
#[must_use]
pub fn marked_count() -> usize {
    MARKED_COUNT.load(Ordering::Relaxed)
}

// ---- scope beyond the marked set (`set_scope`) ----
//
// Besides the instances a mod marks, a mod can widen the event guard:
//
// * **offspring**: an instance whose Create (or Pre-Create) event starts while
//   a contained event or a guarded mod call is running is contained from then
//   on: what a marked instance makes (its shots, beams, effects, parts) and what
//   those make in turn. Kept in a thread-local table that each of the
//   instance's events touches, pruned by age past [`OFFSPRING_MAX`] (the
//   runner never reuses an id, so a dead one only takes room). An object with
//   no Create event anywhere in its ancestry never enters it;
// * **objects**: every instance of the named object indices.
//
// Both apply only to the event kinds the mod names (`kinds`, bit
// `1 << kind`), so a mod can leave e.g. Draw out; marked instances and mod
// calls keep every kind.

/// Object indices the scope can name (the game has fewer than 1,000).
pub const MAX_OBJECTS: usize = 4096;
/// [`set_scope`] flag: contain the offspring of contained events.
pub const SCOPE_OFFSPRING: u32 = ndml_sdk::abi::GUARD_SCOPE_OFFSPRING;

/// One bit per object index whose instances' events are contained; the
/// detour tests it for every event, so it takes no lock.
static OBJECT_BITS: [AtomicU64; MAX_OBJECTS / 64] = [const { AtomicU64::new(0) }; MAX_OBJECTS / 64];
/// Event kinds (bit = kind) offspring and the object scope apply to.
static SCOPE_KINDS: AtomicU32 = AtomicU32::new(0);
static ANY_OBJECTS: AtomicBool = AtomicBool::new(false);
static OFFSPRING_ON: AtomicBool = AtomicBool::new(false);
static OBJECT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Replace the scope: `flags` ([`SCOPE_OFFSPRING`]), the object indices whose
/// instances are contained (out-of-range ones ignored), and the event kinds
/// both apply to. No flag and no object turn it off. Game thread.
pub fn set_scope(flags: u32, objects: impl IntoIterator<Item = i32>, kinds: u32) {
    let mut bits = [0u64; MAX_OBJECTS / 64];
    let mut n = 0;
    for o in objects {
        if let Ok(o) = usize::try_from(o)
            && o < MAX_OBJECTS
            && bits[o / 64] & (1 << (o % 64)) == 0
        {
            bits[o / 64] |= 1 << (o % 64);
            n += 1;
        }
    }
    ANY_OBJECTS.store(false, Ordering::Relaxed);
    for (slot, b) in OBJECT_BITS.iter().zip(bits) {
        slot.store(b, Ordering::Relaxed);
    }
    SCOPE_KINDS.store(kinds, Ordering::Relaxed);
    OBJECT_COUNT.store(n, Ordering::Relaxed);
    ANY_OBJECTS.store(n > 0 && kinds != 0, Ordering::Relaxed);
    let offspring = flags & SCOPE_OFFSPRING != 0 && kinds != 0;
    if !offspring {
        OFFSPRING.with(|o| o.borrow_mut().ids.clear());
    }
    OFFSPRING_ON.store(offspring, Ordering::Relaxed);
}

/// Whether offspring or an object scope is on at all (cheap; test first).
#[must_use]
pub fn any_scope() -> bool {
    ANY_OBJECTS.load(Ordering::Relaxed) || OFFSPRING_ON.load(Ordering::Relaxed)
}

/// Whether the scope's event kinds include `kind`.
#[must_use]
pub fn scope_kind(kind: i32) -> bool {
    u32::try_from(kind).is_ok_and(|k| k < 32 && SCOPE_KINDS.load(Ordering::Relaxed) & (1 << k) != 0)
}

/// Whether instances of `object` are in the object scope.
#[must_use]
pub fn object_contained(object: i32) -> bool {
    ANY_OBJECTS.load(Ordering::Relaxed)
        && usize::try_from(object).is_ok_and(|o| {
            o < MAX_OBJECTS && OBJECT_BITS[o / 64].load(Ordering::Relaxed) & (1 << (o % 64)) != 0
        })
}

/// How many objects the scope names.
#[must_use]
pub fn contained_object_count() -> usize {
    OBJECT_COUNT.load(Ordering::Relaxed)
}

/// Offspring kept before the stale and then the oldest are pruned.
const OFFSPRING_MAX: usize = 4096;
/// Events (of offspring checks) after which an untouched offspring is stale.
const OFFSPRING_STALE: u64 = 1_000_000;

/// Event kinds that start an instance.
const EVENT_CREATE: i32 = 0;
const EVENT_PRE_CREATE: i32 = 14;

/// Instance ids are dense integers; a multiplicative hash is enough.
#[derive(Default, Clone, Copy)]
struct IdHash(u64);

impl std::hash::Hasher for IdHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_i32(&mut self, i: i32) {
        self.0 = u64::from(u32::from_ne_bytes(i.to_ne_bytes())).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

#[derive(Default)]
struct Offspring {
    /// Id -> the clock when it last ran an event.
    ids: HashMap<i32, u64, std::hash::BuildHasherDefault<IdHash>>,
    clock: u64,
    added: u64,
}

impl Offspring {
    fn add(&mut self, id: i32) {
        self.clock += 1;
        self.added += 1;
        self.ids.insert(id, self.clock);
        if self.ids.len() > OFFSPRING_MAX {
            let clock = self.clock;
            self.ids.retain(|_, t| clock - *t < OFFSPRING_STALE);
            if self.ids.len() > OFFSPRING_MAX * 3 / 4 {
                let mut ts: Vec<u64> = self.ids.values().copied().collect();
                ts.sort_unstable();
                let cut = ts[ts.len() / 2];
                self.ids.retain(|_, t| *t >= cut);
            }
        }
    }

    fn touch(&mut self, id: i32) -> bool {
        self.clock += 1;
        let clock = self.clock;
        self.ids.get_mut(&id).map(|t| *t = clock).is_some()
    }
}

thread_local! {
    static OFFSPRING: std::cell::RefCell<Offspring> = std::cell::RefCell::new(Offspring::default());
    /// Contained events running now (any depth).
    static CONTAINED_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Whether the starting event (kind `kind`) of instance `id` is an
/// offspring's: one already known, or one whose Create event starts now inside
/// a contained event or guarded call (it is added then).
#[must_use]
pub fn offspring_event(id: i32, kind: i32) -> bool {
    if !OFFSPRING_ON.load(Ordering::Relaxed) || !scope_kind(kind) {
        return false;
    }
    OFFSPRING.with(|o| {
        let mut o = o.borrow_mut();
        if o.touch(id) {
            return true;
        }
        let creating = kind == EVENT_CREATE || kind == EVENT_PRE_CREATE;
        if creating && (CONTAINED_DEPTH.with(Cell::get) > 0 || in_guarded_call()) {
            o.add(id);
            return true;
        }
        false
    })
}

/// Offspring tracked now, and added in all.
#[must_use]
pub fn offspring_counts() -> (usize, u64) {
    OFFSPRING.with(|o| {
        let o = o.borrow();
        (o.ids.len(), o.added)
    })
}

/// A contained event starts: what [`contained_leave`] restores.
#[must_use]
pub fn contained_enter() -> u32 {
    CONTAINED_DEPTH.with(|d| {
        let n = d.get();
        d.set(n + 1);
        n
    })
}

/// The contained event is done (normally or abandoned).
pub fn contained_leave(saved: u32) {
    CONTAINED_DEPTH.with(|d| d.set(saved));
}

/// Why an event was run inside the guard frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Its instance is marked by a mod.
    Marked,
    /// Its instance was made inside a contained event or a mod call.
    Offspring,
    /// Its object is in the object scope.
    Object,
    /// It ran inside a mod's guarded call.
    InCall,
}

impl Why {
    /// The ABI's `GuardReport::marked` code.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::InCall => 0,
            Self::Marked => 1,
            Self::Object => 2,
            Self::Offspring => 3,
        }
    }

    /// For the log.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Marked => "marked by a mod",
            Self::Offspring => "made by a contained event or mod call",
            Self::Object => "an object in the mod's scope",
            Self::InCall => "run inside a mod call",
        }
    }
}

// ---- event observers ----

/// Observers allowed in total (all mods).
pub const MAX_OBSERVERS: usize = 64;

/// "Call me before and after any event of kind `kind`, number `number`"
/// (`-1` = any), registered by a mod during init and run from the `ExecuteIt`
/// detour, without a hook site per object.
#[derive(Clone, Copy, Debug)]
pub struct Observer {
    pub kind: i32,
    pub number: i32,
    /// The mod's callback (`hooks` makes the call) and its `user` pointer (an
    /// address: the list is shared with the game thread).
    pub cb: ndml_sdk::abi::EventObserverCallback,
    pub user: usize,
    pub mod_id: u32,
}

impl Observer {
    #[must_use]
    pub fn matches(&self, kind: i32, number: i32) -> bool {
        (self.kind == -1 || self.kind == kind) && (self.number == -1 || self.number == number)
    }
}

/// Registration (init only): the list and whether it is frozen.
static OBSERVER_REG: Mutex<(Vec<Observer>, bool)> = Mutex::new((Vec::new(), false));
/// The frozen list the detour reads, lock-free.
static OBSERVERS: OnceLock<Box<[Observer]>> = OnceLock::new();

/// Register an observer during mod init. `HooksFrozen` after boot,
/// `Exhausted` past [`MAX_OBSERVERS`].
pub fn add_observer(o: Observer) -> ndml_sdk::Status {
    let mut reg = OBSERVER_REG.lock().unwrap_or_else(PoisonError::into_inner);
    if reg.1 {
        return ndml_sdk::Status::HooksFrozen;
    }
    if reg.0.len() >= MAX_OBSERVERS {
        return ndml_sdk::Status::Exhausted;
    }
    reg.0.push(o);
    ndml_sdk::Status::Ok
}

/// Drop every observer `mod_id` registered (its init failed). Only before
/// [`freeze_observers`]; returns how many were dropped.
pub fn drop_observers(mod_id: u32) -> usize {
    let mut reg = OBSERVER_REG.lock().unwrap_or_else(PoisonError::into_inner);
    if reg.1 {
        return 0;
    }
    without_mod(&mut reg.0, mod_id)
}

pub(crate) fn without_mod(list: &mut Vec<Observer>, mod_id: u32) -> usize {
    let before = list.len();
    list.retain(|o| o.mod_id != mod_id);
    before - list.len()
}

/// Whether any observer was registered (boot decides on the detour).
#[must_use]
pub fn observers_registered() -> bool {
    !OBSERVER_REG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .0
        .is_empty()
}

/// Freeze registration. `live`: the observers will be called; otherwise they
/// are dropped (the detour or the event globals were not found). Returns how
/// many will run.
pub fn freeze_observers(live: bool) -> usize {
    let mut reg = OBSERVER_REG.lock().unwrap_or_else(PoisonError::into_inner);
    reg.1 = true;
    let list = if live {
        std::mem::take(&mut reg.0)
    } else {
        Vec::new()
    };
    let n = list.len();
    let _ = OBSERVERS.set(list.into_boxed_slice());
    n
}

/// The frozen observers (empty before boot or when they could not run).
#[must_use]
pub fn observers() -> &'static [Observer] {
    OBSERVERS.get().map_or(&[], |b| b)
}

// ---- nesting (game thread) ----

thread_local! {
    /// Events running through the `ExecuteIt` detour, innermost = this value.
    static EVENT_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Guarded mod calls in progress.
    static CALL_DEPTH: Cell<u32> = const { Cell::new(0) };
    /// Mods' "errors expected" scopes open ([`expect`]).
    static EXPECT: Cell<u32> = const { Cell::new(0) };
}

/// A mod opens (`on`) or closes an "errors expected" scope around a call it
/// makes knowing the call may raise GML errors: errors caught while one is
/// open are reported and logged as expected. The SDK closes the scope when
/// its guard object drops; a top-level event starting with one still open (a mod that never closed
/// it) closes them all and says so, so a scope cannot hide later errors.
/// Returns the scopes now open.
#[must_use]
pub fn expect(on: bool) -> u32 {
    EXPECT.with(|e| {
        let n = if on {
            e.get().saturating_add(1)
        } else {
            e.get().saturating_sub(1)
        };
        e.set(n);
        n
    })
}

/// Whether an "errors expected" scope is open.
#[must_use]
pub fn expecting() -> bool {
    EXPECT.with(Cell::get) > 0
}

/// Errors caught inside "errors expected" scopes so far.
static EXPECTED: AtomicU32 = AtomicU32::new(0);

/// An event starts: its depth. Pair with [`event_leave`] (also after a caught
/// error: inner events the error unwound never left).
#[must_use]
pub fn event_enter() -> u32 {
    let n = EVENT_DEPTH.with(|d| {
        let n = d.get() + 1;
        d.set(n);
        n
    });
    // A top-level event (the runner's own dispatch, outside any event or mod
    // call): no mod call is running, so no "expected" scope may be open.
    if n == 1 && !in_guarded_call() && expecting() {
        EXPECT.with(|e| e.set(0));
        log::warn!("GML error guard: an 'errors expected' scope was left open by a mod; closed");
    }
    n
}

/// The event at `depth` is done; anything deeper is gone too.
pub fn event_leave(depth: u32) {
    EVENT_DEPTH.with(|d| d.set(depth.saturating_sub(1)));
}

/// Whether the event at `depth` is the innermost one (the filter's test: an
/// error belongs to the innermost running event).
#[must_use]
pub fn event_is_innermost(depth: u32) -> bool {
    EVENT_DEPTH.with(Cell::get) == depth
}

/// A guarded mod call starts; returns the event depth to restore afterwards.
#[must_use]
pub fn call_enter() -> (u32, u32) {
    let calls = CALL_DEPTH.with(|d| {
        let n = d.get() + 1;
        d.set(n);
        n
    });
    (calls, EVENT_DEPTH.with(Cell::get))
}

/// The guarded call is done (normally or not).
pub fn call_leave((calls, events): (u32, u32)) {
    CALL_DEPTH.with(|d| d.set(calls.saturating_sub(1)));
    EVENT_DEPTH.with(|d| d.set(events));
}

/// Inside a guarded mod call (events started here are contained too).
#[must_use]
pub fn in_guarded_call() -> bool {
    CALL_DEPTH.with(Cell::get) > 0
}

// ---- caught errors ----

/// Where an error was caught.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caught {
    /// An event of a marked instance (or one run inside a mod call) was abandoned.
    Event,
    /// A mod's script/builtin call returned `GmlError`.
    Call,
}

/// One caught GML error.
#[derive(Clone, Debug)]
pub struct Report {
    pub caught: Caught,
    /// The event's instance id (`-1` for a call).
    pub instance: i32,
    /// Its object index (`-1` for a call).
    pub object: i32,
    /// Why the event was guarded (`Why::InCall` for a call too).
    pub why: Why,
    /// The runner's long message ("ERROR in action number 1 of Step Event0 for
    /// object …: …"), or what could be read of it.
    pub text: String,
    /// Caught inside a mod's "errors expected" scope ([`expect`]).
    pub expected: bool,
}

/// Count an error caught inside an "errors expected" scope and log it at
/// info (not as a contained error), rate-limited per site like those.
pub fn log_expected(object: i32, what: &str, line: &str) {
    let total = EXPECTED.fetch_add(1, Ordering::Relaxed) + 1;
    if let Some(n) = log_due(object, line) {
        log::info!(
            "GML error expected by a mod (#{n} at this site, {total} in all): {what}: {line}"
        );
    }
}

/// Rate limit of the loader's own log line per error site (object and
/// message): the first [`LOG_FIRST`] are logged, then every [`LOG_EVERY`]th.
const LOG_FIRST: u32 = 3;
const LOG_EVERY: u32 = 100;

static LOG_SEEN: Mutex<Option<HashMap<(i32, String), u32>>> = Mutex::new(None);

/// Count an error at `(object, text)` and say whether to log it now: `Some(n)`
/// (its count) for the first 3 and then every 100th.
#[must_use]
pub fn log_due(object: i32, text: &str) -> Option<u32> {
    let mut g = LOG_SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    let m = g.get_or_insert_with(HashMap::new);
    let n = m.entry((object, text.to_owned())).or_insert(0);
    *n += 1;
    (*n <= LOG_FIRST || (*n).is_multiple_of(LOG_EVERY)).then_some(*n)
}

/// Kept until a mod drains them (oldest dropped beyond this).
const MAX_REPORTS: usize = 64;

static REPORTS: Mutex<VecDeque<Report>> = Mutex::new(VecDeque::new());
pub fn push_report(r: Report) {
    let mut q = REPORTS.lock().unwrap_or_else(PoisonError::into_inner);
    if q.len() >= MAX_REPORTS {
        q.pop_front();
    }
    q.push_back(r);
}

/// Take every queued report.
#[must_use]
pub fn take_reports() -> Vec<Report> {
    REPORTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drain(..)
        .collect()
}

// ---- deciding in the filter ----

/// An exception as the guard's SEH filter sees it.
#[derive(Clone, Copy, Debug)]
pub struct Thrown {
    /// `ExceptionCode` (MSVC C++: `0xE06D7363`).
    pub code: u32,
    /// `ExceptionInformation[0..4]`: for C++ the magic, the thrown object, its
    /// `ThrowInfo` and the image base the RVAs in it are relative to.
    pub params: [usize; 4],
    pub nparams: usize,
}

/// The game image (base, bytes), to read throw info without raw pointers.
static IMAGE: OnceLock<(usize, &'static [u8])> = OnceLock::new();

pub fn set_image(base: usize, bytes: &'static [u8]) {
    let _ = IMAGE.set((base, bytes));
}

/// MSVC type name of the runner's exception class (NUL-terminated).
const GML_EXCEPTION_TYPE: &[u8] = b".?AVYYGMLException@@\x00";

/// Whether `t` is a GML runtime error: an MSVC C++ exception thrown from the
/// game image whose first catchable type is `YYGMLException`
/// (`ThrowInfo.pCatchableTypeArray` -> `CatchableType.pType` ->
/// `TypeDescriptor.name`, all image-relative).
#[must_use]
pub fn is_gml_error(t: &Thrown) -> bool {
    let Some(&(base, img)) = IMAGE.get() else {
        return false;
    };
    if t.code != 0xE06D_7363 || t.nparams < 4 || t.params[3] != base {
        return false;
    }
    let u32_at = |rva: usize| -> Option<usize> {
        Some(u32::from_le_bytes(img.get(rva..rva + 4)?.try_into().ok()?) as usize)
    };
    let name = || -> Option<bool> {
        let throw_info = t.params[2].checked_sub(base)?;
        let types = u32_at(throw_info + 12)?;
        if u32_at(types)? == 0 {
            return Some(false);
        }
        let first = u32_at(types + 4)?;
        let descriptor = u32_at(first + 4)?;
        let at = descriptor + 16;
        Some(img.get(at..at + GML_EXCEPTION_TYPE.len())? == GML_EXCEPTION_TYPE)
    };
    name().unwrap_or(false)
}

/// Filter decision of an event frame (`arg` = its depth): a GML error raised
/// while this event is the innermost one.
#[must_use]
pub fn decide_event(t: &Thrown, depth: usize) -> bool {
    is_gml_error(t) && event_is_innermost(depth as u32)
}

/// Filter decision of a mod-call frame: any GML error raised inside the call
/// (an event inside the call that is guarded itself takes it first).
#[must_use]
pub fn decide_call(t: &Thrown, _: usize) -> bool {
    is_gml_error(t)
}

// ---- finding what the guard needs ----

/// What the guard needs from the runner, as absolute addresses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sites {
    /// `bool ExecuteIt(CInstance* self, CInstance* other, CCode* code,
    /// RValue* result, int flags)`: runs one compiled event.
    pub execute_it: usize,
    /// `void YYError(const char* fmt, ...)`.
    pub yyerror: usize,
    /// `int32` globals the inner `Perform_Event` saves before an event and
    /// restores after it (recursion depth, current object, event type/number).
    pub event_dwords: Vec<usize>,
    /// Byte globals the event executor wrapper saves and restores.
    pub exec_bytes: Vec<usize>,
    /// `int32` count of the GC root stack (YYC's `try` restores it in `catch`).
    pub gc_root_count: usize,
    /// Which of `event_dwords` hold the current event, if their roles were
    /// recognised (see `event_roles`): set by `Perform_Event` just before
    /// the event's code runs, so readable inside `ExecuteIt`.
    pub roles: Option<EventRoles>,
}

/// The current event's globals (absolute addresses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRoles {
    /// Event type (0 Create, 1 Destroy, 2 Alarm, 3 Step, 7 Other, 8 Draw, ...).
    pub kind: usize,
    /// Event number (user event `n` is Other `10 + n`).
    pub number: usize,
    /// The object whose event code runs (a parent's for an inherited event).
    pub object: usize,
}

/// Roles of the globals `Perform_Event` sets before an event, from where the
/// stored register was loaded in the function body: from `r9d` (the 4th
/// argument, the event type), from `[rsp+disp8]` (the 5th, on the stack: the
/// event number), from `[reg+disp8]` (the found event's object). Stores of a
/// register last loaded from a RIP-relative global (the restores) and of the
/// depth counter are ignored. A tiny decoder over raw bytes: only `mov r32,
/// r/m32` (`[REX] 8B /r`) and `mov [rip+X], r32` (`[REX] 89 /r`).
fn event_roles(
    text: &[u8],
    text_rva: usize,
    from: usize,
    to: usize,
    skip: usize,
) -> Option<EventRoles> {
    #[derive(Clone, Copy, PartialEq)]
    enum Src {
        Type,
        Number,
        Object,
        Other,
    }
    let mut last = [Src::Other; 16];
    let (mut kind, mut number, mut object) = (None, None, None);
    let mut i = from;
    while i + 2 < to {
        let (rex, at) = if (0x40..=0x4F).contains(&text[i]) {
            (text[i], i + 1)
        } else {
            (0, i)
        };
        let (op, m) = (text[at], text[at + 1]);
        let reg = usize::from((m >> 3) & 7) | if rex & 4 != 0 { 8 } else { 0 };
        let (md, rm) = (m >> 6, m & 7);
        // A recognised instruction is consumed whole, so its inner bytes are
        // never read as another one.
        let len = match op {
            0x8B if md == 3 => {
                let src = usize::from(rm) | if rex & 1 != 0 { 8 } else { 0 };
                last[reg] = if src == 9 { Src::Type } else { Src::Other };
                2
            }
            0x8B if md == 1 && rm == 4 && text.get(at + 2) == Some(&0x24) => {
                last[reg] = Src::Number;
                4
            }
            0x8B if md == 1 && rm != 4 => {
                last[reg] = Src::Object;
                3
            }
            0x8B if md == 0 && rm == 5 => {
                last[reg] = Src::Other;
                6
            }
            0x89 if md == 0 && rm == 5 => {
                if let Some(t) = rel32(text, at + 2).and_then(|r| target(text_rva, at + 6, r))
                    && t != skip
                {
                    match last[reg] {
                        Src::Type => kind = Some(t),
                        Src::Number => number = Some(t),
                        Src::Object => object = Some(t),
                        Src::Other => {}
                    }
                }
                6
            }
            _ => {
                i += 1;
                continue;
            }
        };
        i = at + len;
    }
    let roles = EventRoles {
        kind: kind?,
        number: number?,
        object: object?,
    };
    (roles.kind != roles.number && roles.number != roles.object && roles.kind != roles.object)
        .then_some(roles)
}

/// The error `Perform_Event` raises past 32 nested events: unique, and only
/// in the inner `Perform_Event` functions.
const RECURSION_MSG: &[u8] = b"PerformEvent recursion depth failure";
/// How far past the string's `lea` a `Perform_Event` body is searched.
const MAX_BODY: usize = 0x600;

fn rel32(b: &[u8], at: usize) -> Option<isize> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as isize)
}

/// `rva` of the byte at `i` of a slice starting at `base_rva`, plus `rel`.
fn target(base_rva: usize, end: usize, rel: isize) -> Option<usize> {
    (base_rva + end).checked_add_signed(rel)
}

/// Body from `start` to the first `ret; int3` (end of a function followed by
/// padding), at most [`MAX_BODY`] bytes.
fn body_end(text: &[u8], start: usize) -> usize {
    let cap = (start + MAX_BODY).min(text.len());
    (start..cap.saturating_sub(1))
        .find(|&i| text[i] == 0xC3 && text[i + 1] == 0xCC)
        .map_or(cap, |i| i + 1)
}

/// `call rel32` targets in `text[from..to]` (RVAs), in order.
fn calls(text: &[u8], text_rva: usize, from: usize, to: usize) -> Vec<(usize, usize)> {
    (from..to.saturating_sub(4))
        .filter(|&i| text[i] == 0xE8)
        .filter_map(|i| Some((i, target(text_rva, i + 5, rel32(text, i + 1)?)?)))
        .collect()
}

/// RIP-relative `mov dword [rip+X], r32` (`89 /r`, optional REX.R `44`) and
/// `mov byte [rip+X], r8` / `mov byte [rip+X], imm8` (`88 /r`, `C6 05 … ib`)
/// targets in `text[from..to]` that lie in `data` (RVAs).
fn stores(text: &[u8], text_rva: usize, from: usize, to: usize, data: &pe::Section) -> StoreSets {
    let mut s = StoreSets::default();
    for i in from..to.saturating_sub(6) {
        let rip = |m: u8| m & 0xC7 == 0x05;
        let (set, end) = match text[i] {
            0x89 if rip(text[i + 1]) => (&mut s.dwords, i + 6),
            0x88 if rip(text[i + 1]) => (&mut s.bytes, i + 6),
            0xC6 if text[i + 1] == 0x05 => (&mut s.bytes, i + 7),
            _ => continue,
        };
        if let Some(t) = rel32(text, i + 2).and_then(|r| target(text_rva, end, r))
            && data.contains_rva(t)
        {
            set.insert(t);
        }
    }
    s
}

#[derive(Default, Debug)]
struct StoreSets {
    dwords: HashSet<usize>,
    bytes: HashSet<usize>,
}

/// Find [`Sites`] by shape: the inner `Perform_Event` functions reference the
/// recursion message; they bump a depth counter (`inc ecx; mov [D], ecx; cmp
/// ecx, 0x20`), save and restore the current object/event globals, and call a
/// small wrapper that saves a flag byte and calls `ExecuteIt`, which raises
/// with `YYError` when the code fails. The GC root stack count is the global
/// of the tiny pop routine (`mov eax,[C]; sub eax,ecx; mov ecx,0; cmovs
/// eax,ecx; mov [C],eax; ret`).
#[allow(clippy::too_many_lines)] // one linear recipe; splitting it hides the order
pub fn find_sites(image: Image<'_>, layout: &Layout) -> Result<Sites, String> {
    let sec = |n: &str| layout.section(n).ok_or(format!("no {n} section"));
    let (text_s, rdata_s, data_s) = (sec(".text")?, sec(".rdata")?, sec(".data")?);
    let text = image
        .bytes
        .get(text_s.rva..text_s.rva + text_s.size)
        .ok_or("text out of image")?;
    let rdata = image
        .bytes
        .get(rdata_s.rva..rdata_s.rva + rdata_s.size)
        .ok_or("rdata out of image")?;
    let msg = rdata
        .windows(RECURSION_MSG.len())
        .position(|w| w == RECURSION_MSG)
        .filter(|&i| i > 0 && rdata[i - 1] == 0)
        .map(|i| rdata_s.rva + i)
        .ok_or("recursion message not found")?;
    // `lea rcx, [rip+msg]`
    let leas: Vec<usize> = (0..text.len().saturating_sub(7))
        .filter(|&i| text[i] == 0x48 && text[i + 1] == 0x8D && text[i + 2] == 0x0D)
        .filter(|&i| rel32(text, i + 3).and_then(|r| target(text_s.rva, i + 7, r)) == Some(msg))
        .collect();
    if leas.is_empty() || leas.len() > 4 {
        return Err(format!(
            "{} references to the recursion message",
            leas.len()
        ));
    }
    let mut depth = None;
    let mut yyerror = None;
    let mut dwords: Option<HashSet<usize>> = None;
    let mut common_calls: Option<HashSet<usize>> = None;
    // Event roles per Perform_Event function: all must agree (else none).
    let mut roles: Option<Option<EventRoles>> = None;
    for &lea in &leas {
        let (begin, _) = pe::function_bounds(image.bytes, layout, text_s.rva + lea)
            .ok_or("no unwind entry for Perform_Event")?;
        let from = begin - text_s.rva;
        let to = body_end(text, lea);
        // YYError: the call right after the message.
        let after = calls(text, text_s.rva, lea + 7, (lea + 7 + 16).min(to));
        let y = after
            .first()
            .map(|c| c.1)
            .ok_or("no call after the message")?;
        if *yyerror.get_or_insert(y) != y {
            return Err("Perform_Event functions call different error routines".into());
        }
        // inc ecx; mov [rip+D], ecx; cmp ecx, 0x20
        let d = (from..to.saturating_sub(11))
            .find(|&i| {
                text[i..i + 4] == [0xFF, 0xC1, 0x89, 0x0D]
                    && text[i + 8..i + 11] == [0x83, 0xF9, 0x20]
            })
            .and_then(|i| target(text_s.rva, i + 8, rel32(text, i + 4)?))
            .ok_or("depth counter not found")?;
        if *depth.get_or_insert(d) != d {
            return Err("Perform_Event functions use different depth counters".into());
        }
        // The body may be split into chained unwind entries (shrink-wrapped
        // saves): the arguments are moved in the first one.
        let mut start = begin;
        for _ in 0..4 {
            match pe::function_bounds(image.bytes, layout, start - 1) {
                Some((b, e)) if e == start => start = b,
                _ => break,
            }
        }
        let r = event_roles(text, text_s.rva, start - text_s.rva, to, d);
        roles = Some(match roles {
            None => r,
            Some(prev) => prev.filter(|p| Some(*p) == r),
        });
        let st = stores(text, text_s.rva, from, to, data_s);
        dwords = Some(match dwords {
            None => st.dwords,
            Some(prev) => prev.intersection(&st.dwords).copied().collect(),
        });
        let targets: HashSet<usize> = calls(text, text_s.rva, from, to)
            .into_iter()
            .map(|c| c.1)
            .filter(|&t| t != y)
            .collect();
        common_calls = Some(match common_calls {
            None => targets,
            Some(prev) => prev.intersection(&targets).copied().collect(),
        });
    }
    let (depth, yyerror) = (depth.ok_or("no depth")?, yyerror.ok_or("no YYError")?);
    let dwords = dwords.unwrap_or_default();
    if !dwords.contains(&depth) || !(2..=8).contains(&dwords.len()) {
        return Err(format!(
            "{} event globals (depth among them: {})",
            dwords.len(),
            dwords.contains(&depth)
        ));
    }
    // The executor wrapper: a common callee with exactly one call, to a
    // function that calls YYError.
    let mut found = None;
    let mut cands: Vec<usize> = common_calls.unwrap_or_default().into_iter().collect();
    cands.sort_unstable();
    for c in cands {
        let Some(off) = c.checked_sub(text_s.rva).filter(|&o| o < text.len()) else {
            continue;
        };
        let end = body_end(text, off).min(off + 0x80);
        let inner = calls(text, text_s.rva, off, end);
        let [(_, x)] = inner[..] else { continue };
        let Some(xo) = x.checked_sub(text_s.rva).filter(|&o| o < text.len()) else {
            continue;
        };
        let x_end = body_end(text, xo).min(xo + 0x60);
        if calls(text, text_s.rva, xo, x_end)
            .iter()
            .any(|cl| cl.1 == yyerror)
        {
            if found.is_some() {
                return Err("several executor candidates".into());
            }
            found = Some((x, stores(text, text_s.rva, off, end, data_s).bytes));
        }
    }
    let (execute_it, bytes) = found.ok_or("ExecuteIt not found")?;
    let gc = find_gc_root_count(text, text_s.rva, data_s).ok_or("GC root stack not found")?;
    let abs = |v: usize| image.base + v;
    let mut event_dwords: Vec<usize> = dwords.into_iter().map(abs).collect();
    event_dwords.sort_unstable();
    let mut exec_bytes: Vec<usize> = bytes.into_iter().map(abs).collect();
    exec_bytes.sort_unstable();
    Ok(Sites {
        execute_it: abs(execute_it),
        yyerror: abs(yyerror),
        event_dwords,
        exec_bytes,
        gc_root_count: abs(gc),
        roles: roles.flatten().map(|r| EventRoles {
            kind: abs(r.kind),
            number: abs(r.number),
            object: abs(r.object),
        }),
    })
}

/// The pop routine `mov eax,[rip+C]; sub eax,ecx; mov ecx,0; cmovs eax,ecx;
/// mov [rip+C],eax; ret`; exactly one in the runner.
fn find_gc_root_count(text: &[u8], text_rva: usize, data: &pe::Section) -> Option<usize> {
    const MID: [u8; 9] = [0x2B, 0xC1, 0xB9, 0, 0, 0, 0, 0x0F, 0x48];
    let mut found = None;
    for i in 0..text.len().saturating_sub(23) {
        if text[i] != 0x8B || text[i + 1] != 0x05 || text[i + 6..i + 15] != MID {
            continue;
        }
        if text[i + 15] != 0xC1
            || text[i + 16] != 0x89
            || text[i + 17] != 0x05
            || text[i + 22] != 0xC3
        {
            continue;
        }
        let a = target(text_rva, i + 6, rel32(text, i + 2)?)?;
        let b = target(text_rva, i + 22, rel32(text, i + 18)?)?;
        if a == b && data.contains_rva(a) {
            if found.is_some() {
                return None;
            }
            found = Some(a);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pe::Section;

    #[test]
    fn depth_tracks_innermost() {
        let a = event_enter();
        let b = event_enter();
        assert!(event_is_innermost(b) && !event_is_innermost(a));
        // An error caught at `a` unwound `b` without its leave.
        event_leave(a);
        let c = event_enter();
        assert_eq!(c, a);
        event_leave(c);
        assert_eq!(EVENT_DEPTH.with(Cell::get), a - 1);
    }

    #[test]
    fn calls_restore_event_depth() {
        let e = event_enter();
        let tok = call_enter();
        assert!(in_guarded_call());
        let _inner = event_enter();
        call_leave(tok);
        assert!(!in_guarded_call() && event_is_innermost(e));
        event_leave(e);
    }

    #[test]
    fn expect_scopes_nest_and_cannot_be_left_open() {
        assert!(!expecting());
        let tok = call_enter();
        assert_eq!(expect(true), 1);
        assert_eq!(expect(true), 2);
        // Events the call runs keep the scope.
        let e = event_enter();
        assert!(expecting());
        event_leave(e);
        assert_eq!(expect(false), 1);
        call_leave(tok);
        // Left open: the runner's next top-level event closes it.
        let e = event_enter();
        assert!(!expecting());
        event_leave(e);
        assert_eq!(expect(false), 0);
    }

    #[test]
    fn marks() {
        set_marked([3, 5]);
        assert!(is_marked(5) && !is_marked(4));
        set_marked([]);
        assert!(!is_marked(5) && marked_count() == 0);
        // Past the table's half load into the overflow set, duplicates once.
        let n = i32::try_from(MARK_TABLE_MAX).unwrap_or(i32::MAX) + 300;
        set_marked((100_000..100_000 + n).chain([100_000, 100_001]));
        assert_eq!(marked_count(), n as usize);
        assert!((100_000..100_000 + n).all(is_marked));
        assert!(!is_marked(99_999) && !is_marked(100_000 + n));
        // A smaller set clears the old one completely.
        set_marked([7]);
        assert!(is_marked(7) && !is_marked(100_000) && !is_marked(100_000 + n - 1));
        assert_eq!(marked_count(), 1);
        set_marked([]);
        assert!(!is_marked(7));
    }

    #[test]
    fn scope_objects_and_offspring() {
        const STEP: i32 = 3;
        const DRAW: i32 = 8;
        let kinds = !(1u32 << DRAW);
        set_scope(SCOPE_OFFSPRING, [742, 5000, -1, 742], kinds);
        assert_eq!(contained_object_count(), 1);
        assert!(any_scope() && object_contained(742) && !object_contained(741));
        assert!(scope_kind(STEP) && !scope_kind(DRAW) && !scope_kind(-1));
        // Not created inside anything contained: not an offspring.
        assert!(!offspring_event(200_001, EVENT_CREATE));
        // Created inside a contained event: an offspring from then on, for
        // the scope's kinds only; and what it makes in turn.
        let saved = contained_enter();
        assert!(offspring_event(200_002, EVENT_CREATE));
        contained_leave(saved);
        assert!(offspring_event(200_002, STEP) && !offspring_event(200_002, DRAW));
        assert!(!offspring_event(200_001, STEP));
        let tok = call_enter();
        assert!(offspring_event(200_003, EVENT_PRE_CREATE));
        call_leave(tok);
        // Pruned by age past the cap, the recently run kept.
        for i in 0..i32::try_from(OFFSPRING_MAX).unwrap_or(0) {
            let saved = contained_enter();
            assert!(offspring_event(300_000 + i, EVENT_CREATE));
            contained_leave(saved);
            assert!(offspring_event(200_002, STEP));
        }
        let (n, added) = offspring_counts();
        assert!(n <= OFFSPRING_MAX && added >= 2 + OFFSPRING_MAX as u64);
        assert!(offspring_event(200_002, STEP) && !offspring_event(200_003, STEP));
        // Off clears it.
        set_scope(0, [], kinds);
        assert!(!any_scope() && !object_contained(742) && !offspring_event(200_002, STEP));
        assert_eq!(offspring_counts().0, 0);
    }

    #[test]
    fn log_rate_limit() {
        let due: Vec<u32> = (0..250).filter_map(|_| log_due(-7, "x")).collect();
        assert_eq!(due, [1, 2, 3, 100, 200]);
        assert_eq!(log_due(-7, "y"), Some(1));
    }

    fn rel(from_end: usize, to: usize) -> [u8; 4] {
        ((to as isize - from_end as isize) as i32).to_le_bytes()
    }

    /// The shape the matcher looks for: `mov eax, [rip+c]; sub eax, ecx;
    /// mov ecx, 0; cmovs eax, ecx; mov [rip+c], eax; ret`.
    #[test]
    fn gc_pop_shape() {
        let data = Section {
            name: ".data".into(),
            rva: 0x9000,
            size: 0x1000,
        };
        let mut t = vec![0xCCu8; 0x40];
        let at = 0x10;
        let c = 0x9010;
        let mut b = vec![0x8B, 0x05];
        b.extend(rel(0x1000 + at + 6, c));
        b.extend([0x2B, 0xC1, 0xB9, 0, 0, 0, 0, 0x0F, 0x48, 0xC1, 0x89, 0x05]);
        b.extend(rel(0x1000 + at + 22, c));
        b.push(0xC3);
        t[at..at + b.len()].copy_from_slice(&b);
        assert_eq!(find_gc_root_count(&t, 0x1000, &data), Some(c));
        // Different globals: not the pop routine.
        t[at + 18] ^= 1;
        assert_eq!(find_gc_root_count(&t, 0x1000, &data), None);
    }

    #[test]
    fn store_shapes() {
        let data = Section {
            name: ".data".into(),
            rva: 0x9000,
            size: 0x1000,
        };
        let mut t = vec![0x90u8; 0x40];
        // mov [rip+0x9100], r14d (44 89 35 rel32)
        t[0..3].copy_from_slice(&[0x44, 0x89, 0x35]);
        t[3..7].copy_from_slice(&rel(0x1000 + 7, 0x9100));
        // mov byte [rip+0x9200], 0 (C6 05 rel32 00)
        t[8..10].copy_from_slice(&[0xC6, 0x05]);
        t[10..14].copy_from_slice(&rel(0x1000 + 15, 0x9200));
        t[14] = 0;
        // mov [rip+0x100], eax: outside .data, ignored
        t[16..18].copy_from_slice(&[0x89, 0x05]);
        t[18..22].copy_from_slice(&rel(0x1000 + 22, 0x100));
        let s = stores(&t, 0x1000, 0, t.len(), &data);
        assert_eq!(s.dwords, HashSet::from([0x9100]));
        assert_eq!(s.bytes, HashSet::from([0x9200]));
    }
}
