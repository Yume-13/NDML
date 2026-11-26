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

//! Hook sites, the per-site thunks, and chain dispatch.
//!
//! Each hooked GML function ("site") gets one inline detour pointing at a
//! monomorphised thunk (`event_thunk::<N>` / `script_thunk::<N>`), so the site
//! index is baked into the code and no runtime code generation is needed.
//! The thunk builds a [`HookCall`] and walks the chain of mod callbacks; the
//! last link calls the original through the detour's trampoline.
//!
//! Lifecycle: mods register during their init (`Registry` behind a mutex),
//! then [`install`] freezes everything into an immutable `SITES` slice and
//! enables the detours. Dispatch never takes a lock.
//!
//! Diagnostics: every site counts its calls, and at `debug` log level a summary
//! of the last [`STATS_PERIOD`] is logged (`hook calls in last 60s: …`), so a
//! hook that stops firing (e.g. after a new run) shows up in the log.
//!
//! Unsafe because it patches game code (inline detours) and calls the original
//! functions through raw trampolines.

#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ndml_sdk::abi::{CInstance, EventSeen, HookCall, HookCallback, RValue, Status};
use seq_macro::seq;

use crate::detour::{self, Calls, PATCH_LEN, Plan, Refusal};
use crate::win;

// ---- Inline detours: the memory side (the decoding is `crate::detour`) ----

/// Why a detour was not built.
#[derive(Debug)]
pub enum DetourError {
    /// The function's first bytes cannot be moved (`crate::detour`).
    Refused { refusal: Refusal, bytes: Vec<u8> },
    /// Memory: the target unreadable, no room within reach, a failed write.
    Failed(String),
}

impl std::fmt::Display for DetourError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { refusal, bytes } => write!(
                f,
                "its first bytes cannot be moved ({refusal}; first bytes {})",
                detour::hex(bytes.get(..16).unwrap_or(bytes))
            ),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

/// Read `target`'s first bytes and decide what a detour there would copy.
fn detour_plan(target: usize, calls: Calls) -> Result<(Vec<u8>, Plan), DetourError> {
    let code = win::read_code(target, detour::READ_LEN)
        .ok_or_else(|| DetourError::Failed("the target is not executable code".to_owned()))?;
    match detour::plan(&code, target as u64, calls) {
        Ok(plan) => Ok((code, plan)),
        Err(refusal) => Err(DetourError::Refused {
            refusal,
            bytes: code,
        }),
    }
}

/// An inline detour, built: its relay and trampoline written, the target not
/// patched until [`Detour::enable`]. Lives for the process (nothing unhooks).
#[derive(Debug)]
pub struct Detour {
    target: usize,
    patch: [u8; PATCH_LEN],
    trampoline: usize,
    plan: Plan,
}

impl Detour {
    /// Build a detour from `target` to `replacement`: refused when the
    /// target's first bytes cannot be moved, failed when memory cannot be
    /// had; either way nothing at the target has changed.
    pub fn new(target: usize, replacement: usize, calls: Calls) -> Result<Self, DetourError> {
        let (code, plan) = detour_plan(target, calls)?;
        let failed = |why: &str| DetourError::Failed(why.to_owned());
        // The relay, padded to 16 bytes, then the trampoline.
        let relay_at =
            win::alloc_code_near(target, 16 + detour::TRAMPOLINE_MAX, detour::NEAR as usize)
                .ok_or_else(|| failed("no free memory within reach of the target"))?;
        let trampoline = relay_at + 16;
        let tramp =
            detour::emit(&plan, &code, target as u64, trampoline as u64).map_err(|refusal| {
                DetourError::Refused {
                    refusal,
                    bytes: code.clone(),
                }
            })?;
        let patch = detour::patch(target as u64, relay_at as u64)
            .ok_or_else(|| failed("the relay is out of reach of the target"))?;
        let mut block = detour::relay(replacement as u64).to_vec();
        block.resize(16, 0xCC);
        block.extend_from_slice(&tramp);
        // SAFETY: fresh memory from `alloc_code_near`, which nothing jumps to
        // until the target is patched; `block` is the relay and a trampoline
        // built for exactly this address.
        unsafe { win::write_code(relay_at, &block) }.map_err(DetourError::Failed)?;
        Ok(Self {
            target,
            patch,
            trampoline,
            plan,
        })
    }

    /// The trampoline: calling it runs the original function.
    #[must_use]
    pub fn trampoline(&self) -> usize {
        self.trampoline
    }

    /// What the trampoline copied.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Patch the target: from now on calls go to the replacement.
    ///
    /// # Safety
    /// No thread may be running the target's first [`PATCH_LEN`] bytes while
    /// they are written (boot: game code not started), and the trampoline
    /// must be published wherever the replacement reads it.
    pub unsafe fn enable(&self) -> Result<(), String> {
        // SAFETY: the caller's contract; the patch is one `jmp rel32` over
        // whole instructions the trampoline copied (or padding).
        unsafe { win::write_code(self.target, &self.patch) }
    }
}

/// A registration refused: the status the mod gets and, when the function
/// cannot be detoured, why.
#[derive(Debug)]
pub struct Refused {
    pub status: Status,
    pub why: Option<String>,
}

impl From<Status> for Refused {
    fn from(status: Status) -> Self {
        Self { status, why: None }
    }
}

/// Maximum number of distinct hooked functions, events and scripts together.
///
/// One registry shared by every mod: each site takes one slot whatever its kind.
/// The thunk tables below are generated with the same count; `seq!` needs a
/// literal, so the number is repeated there, and the tables' declared type
/// `[_; MAX_SITES]` makes any mismatch a compile error.
pub const MAX_SITES: usize = 512;

/// How often the call-count summary is logged (debug level).
pub const STATS_PERIOD: Duration = Duration::from_secs(60);
/// Calls between clock checks for the summary (keeps the hot path cheap).
const STATS_CHECK_EVERY: u64 = 4096;

/// Calls per site since the last summary.
static CALLS: [AtomicU64; MAX_SITES] = [const { AtomicU64::new(0) }; MAX_SITES];
static TOTAL_CALLS: AtomicU64 = AtomicU64::new(0);
static LAST_REPORT: Mutex<Option<Instant>> = Mutex::new(None);

/// Count a call at site `n`; occasionally log the per-site summary.
fn count_call(n: usize) {
    if let Some(c) = CALLS.get(n) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    if TOTAL_CALLS
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(STATS_CHECK_EVERY)
    {
        maybe_report();
    }
}

fn maybe_report() {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let Ok(mut last) = LAST_REPORT.try_lock() else {
        return;
    };
    let now = Instant::now();
    let Some(prev) = *last else {
        *last = Some(now);
        return;
    };
    if now.duration_since(prev) < STATS_PERIOD {
        return;
    }
    *last = Some(now);
    drop(last);
    let Some(sites) = SITES.get() else { return };
    let parts: Vec<String> = sites
        .iter()
        .enumerate()
        .map(|(i, s)| (s, CALLS[i].swap(0, Ordering::Relaxed)))
        .filter(|&(_, n)| n > 0)
        .map(|(s, n)| format!("{} {n}", short_name(&s.name)))
        .collect();
    let silent = sites.len() - parts.len();
    log::debug!(
        "hook calls in last {}s ({silent} silent): {}",
        now.duration_since(prev).as_secs(),
        parts.join(", ")
    );
}

/// Whether any hooked GML function has run yet. Once true, the runner has
/// finished start-up (its builtin tables are filled), so lazy table discovery
/// can trust what it finds.
pub fn dispatched() -> bool {
    TOTAL_CALLS.load(Ordering::Relaxed) > 0
}

/// `gml_Object_oPlayer_Step_0` → `oPlayer_Step_0`, `gml_Script_x` → `x()`.
fn short_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("gml_Object_") {
        rest.to_owned()
    } else if let Some(rest) = name.strip_prefix("gml_Script_") {
        format!("{rest}()")
    } else {
        name.to_owned()
    }
}

type EventFn = unsafe extern "C-unwind" fn(*mut CInstance, *mut CInstance);
type ScriptFn = unsafe extern "C-unwind" fn(
    *mut CInstance,
    *mut CInstance,
    *mut RValue,
    i32,
    *mut *mut RValue,
) -> *mut RValue;
/// `void routine(RValue* result, CInstance* self, CInstance* other, int argc,
/// RValue* args)` (a builtin function; `args` contiguous).
type BuiltinFn =
    unsafe extern "C-unwind" fn(*mut RValue, *mut CInstance, *mut CInstance, i32, *mut RValue);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `void gml_Object_<obj>_<Event>_<n>(CInstance* self, CInstance* other)`
    Event,
    /// `RValue* gml_Script_<name>(self, other, RValue* result, int argc, RValue** argv)`
    Script,
    /// A builtin function, named `builtin:<name>`: [`BuiltinFn`].
    Builtin,
}

impl Kind {
    fn of(name: &str) -> Option<Self> {
        if name.starts_with("gml_Object_") {
            Some(Self::Event)
        } else if name.starts_with("gml_Script_") {
            Some(Self::Script)
        } else if name.starts_with(BUILTIN_PREFIX) {
            Some(Self::Builtin)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Link {
    cb: HookCallback,
    user: *mut c_void,
    mod_id: u32,
}

#[derive(Debug)]
struct Site {
    name: String,
    kind: Kind,
    target: usize,
    chain: Vec<Link>,
    trampoline: usize,
}

// SAFETY: `user` pointers come from the SDK, which requires `Send + Sync` hook
// closures; sites are immutable once published.
unsafe impl Send for Site {}
// SAFETY: as above.
unsafe impl Sync for Site {}

#[derive(Debug, Default)]
struct Registry {
    sites: Vec<Site>,
    frozen: bool,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    sites: Vec::new(),
    frozen: false,
});
static SITES: OnceLock<Box<[Site]>> = OnceLock::new();

/// Add `cb` to the chain for `name`. Chains run in registration order
/// (= mod load order): the first registered callback is outermost. A function
/// the detour cannot move returns `Unsupported` with the reason. A detour
/// that cannot be built at [`install`] is logged and left out.
pub fn register(
    index: &crate::yyc::RunnerIndex,
    mod_id: u32,
    name: &str,
    cb: HookCallback,
    user: *mut c_void,
) -> Result<(), Refused> {
    let Some(kind) = Kind::of(name) else {
        return Err(Status::Unsupported.into());
    };
    let target = match kind {
        Kind::Builtin => crate::host::builtin_routine(&name[BUILTIN_PREFIX.len()..])?,
        Kind::Event | Kind::Script => match index.functions.get(name) {
            Some(&t) => t,
            None => return Err(Status::NotFound.into()),
        },
    };
    if let Err(e) = detour_plan(target, Calls::Refuse) {
        return Err(Refused {
            status: Status::Unsupported,
            why: Some(e.to_string()),
        });
    }
    let link = Link { cb, user, mod_id };
    match REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .add(name, kind, target, link)
    {
        Status::Ok => Ok(()),
        s => Err(s.into()),
    }
}

/// Drop every hook `mod_id` registered: its init failed, so none of it may
/// run (a hook registered before the failing step would otherwise run with
/// half-built state). Only before [`install`]; returns how many were dropped.
/// The callbacks' `user` pointers are left as they are (never called, never
/// freed: they belong to the mod's allocator, and its DLL stays mapped).
pub fn drop_mod(mod_id: u32) -> usize {
    REGISTRY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .drop_mod(mod_id)
}

impl Registry {
    /// See [`drop_mod`]. A site left with no link goes (site indices are
    /// given out at [`install`], so nothing refers to it yet).
    fn drop_mod(&mut self, mod_id: u32) -> usize {
        if self.frozen {
            return 0;
        }
        let before: usize = self.sites.iter().map(|s| s.chain.len()).sum();
        for site in &mut self.sites {
            site.chain.retain(|l| l.mod_id != mod_id);
        }
        self.sites.retain(|s| !s.chain.is_empty());
        before - self.sites.iter().map(|s| s.chain.len()).sum::<usize>()
    }

    /// Chain `link` onto the site for `target`, creating the site if needed.
    /// Refuses once frozen, or when a new site would exceed [`MAX_SITES`].
    fn add(&mut self, name: &str, kind: Kind, target: usize, link: Link) -> Status {
        if self.frozen {
            return Status::HooksFrozen;
        }
        if let Some(site) = self.sites.iter_mut().find(|s| s.target == target) {
            site.chain.push(link);
            return Status::Ok;
        }
        if self.sites.len() >= MAX_SITES {
            return Status::Exhausted;
        }
        self.sites.push(Site {
            name: name.to_owned(),
            kind,
            target,
            chain: vec![link],
            trampoline: 0,
        });
        Status::Ok
    }
}

// The literal must equal `MAX_SITES`; the declared array types enforce it at
// compile time.
seq!(N in 0..512 {
    static EVENT_THUNKS: [EventFn; MAX_SITES] = [#(event_thunk::<N>,)*];
    static SCRIPT_THUNKS: [ScriptFn; MAX_SITES] = [#(script_thunk::<N>,)*];
    static BUILTIN_THUNKS: [BuiltinFn; MAX_SITES] = [#(builtin_thunk::<N>,)*];
});

// `HookCall::site` is a `u32` and the thunks store `N as u32`.
const _: () = assert!(MAX_SITES <= u32::MAX as usize);

/// Freeze registration and enable all detours. Returns the number of sites
/// active. A site whose detour cannot be built or enabled is logged
/// (`hook <name> refused: ...`) and left out; the others are installed.
pub fn install() -> Result<usize, String> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.frozen = true;
    let mut sites = std::mem::take(&mut reg.sites);
    drop(reg);

    let detours = build_detours(&mut sites, |i, kind| match kind {
        Kind::Event => EVENT_THUNKS[i] as usize,
        Kind::Script => SCRIPT_THUNKS[i] as usize,
        Kind::Builtin => BUILTIN_THUNKS[i] as usize,
    });
    let names: Vec<String> = detours
        .iter()
        .map(|(i, _)| sites[*i].name.clone())
        .collect();
    // Publish before enabling: a thunk may run the instant its detour is live.
    // A site left out keeps trampoline 0 and is never entered.
    SITES
        .set(sites.into_boxed_slice())
        .map_err(|_| "hooks installed twice".to_owned())?;
    let mut count = 0;
    for ((_, d), name) in detours.iter().zip(&names) {
        // SAFETY: game threads are not yet running GML (this runs in the entry
        // hook); the site with this trampoline is published above.
        match unsafe { d.enable() } {
            Ok(()) => count += 1,
            Err(e) => log::warn!("hook {name} refused: enabling its detour failed ({e})"),
        }
    }
    Ok(count)
}

/// Build each site's detour to its thunk (`thunk(index, kind)`), setting its
/// trampoline. A site whose detour cannot be built is logged (`hook <name>
/// refused: ...`) and left out of the result; nothing is enabled here.
fn build_detours(sites: &mut [Site], thunk: impl Fn(usize, Kind) -> usize) -> Vec<(usize, Detour)> {
    let mut detours = Vec::with_capacity(sites.len());
    for (i, site) in sites.iter_mut().enumerate() {
        let thunk = thunk(i, site.kind);
        let mods: Vec<u32> = site.chain.iter().map(|l| l.mod_id).collect();
        // `target` is the entry of a compiled GML function found in the runner
        // table (or a builtin's routine), and `thunk` has the matching signature.
        match Detour::new(site.target, thunk, Calls::Refuse) {
            Ok(d) => {
                log::debug!(
                    "hook site {} @ {:#x} ({:?}) mods {mods:?}: {} bytes copied, relocated {}",
                    site.name,
                    site.target,
                    site.kind,
                    d.plan().copied,
                    d.plan().relocated()
                );
                site.trampoline = d.trampoline();
                detours.push((i, d));
            }
            Err(e) => log::warn!("hook {} refused: {e}", site.name),
        }
    }
    detours
}

/// Advance the chain for `call`: next mod callback, or the original function.
///
/// # Safety
/// `call` must be a live `HookCall` created by one of this module's thunks.
pub unsafe extern "C-unwind" fn call_next(call: *mut HookCall) {
    let Some(sites) = SITES.get() else { return };
    // SAFETY: caller contract.
    let c = unsafe { &mut *call };
    let Some(site) = sites.get(c.site as usize) else {
        return;
    };
    let i = c.next as usize;
    if i < site.chain.len() {
        c.next += 1;
        let link = site.chain[i];
        // SAFETY: callback/user pair registered together by the SDK.
        unsafe { (link.cb)(call, link.user) };
    } else if i == site.chain.len() {
        c.next += 1; // the original runs at most once
        match site.kind {
            Kind::Event => {
                // SAFETY: trampoline of an event-signature function.
                let f: EventFn = unsafe { std::mem::transmute(site.trampoline) };
                // SAFETY: forwarding the game's own arguments.
                unsafe { f(c.self_inst, c.other_inst) };
            }
            Kind::Script => {
                // SAFETY: trampoline of a script-signature function.
                let f: ScriptFn = unsafe { std::mem::transmute(site.trampoline) };
                // SAFETY: forwarding the game's own arguments.
                c.ret = unsafe { f(c.self_inst, c.other_inst, c.result, c.argc, c.argv) };
            }
            Kind::Builtin => {
                // SAFETY: trampoline of a builtin routine.
                let f: BuiltinFn = unsafe { std::mem::transmute(site.trampoline) };
                // The thunk built `argv` over the runner's contiguous array:
                // its first entry is the array.
                let args = if c.argc > 0 && !c.argv.is_null() {
                    // SAFETY: `argv` has `argc` entries (thunk contract).
                    unsafe { *c.argv }
                } else {
                    core::ptr::null_mut()
                };
                // SAFETY: forwarding the runner's own arguments.
                unsafe { f(c.result, c.self_inst, c.other_inst, c.argc, args) };
            }
        }
    }
}

/// Hook names for builtin functions: `builtin:<name>` (e.g. `builtin:instance_copy`).
pub const BUILTIN_PREFIX: &str = "builtin:";

/// Arguments a builtin thunk lists on its stack; more go to the heap.
const BUILTIN_ARGS_ON_STACK: usize = 16;

#[allow(clippy::similar_names)] // argc/args mirror the runner's routine signature
unsafe extern "C-unwind" fn builtin_thunk<const N: usize>(
    result: *mut RValue,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    argc: i32,
    args: *mut RValue,
) {
    count_call(N);
    let n = if args.is_null() {
        0
    } else {
        usize::try_from(argc).unwrap_or(0)
    };
    // The SDK reads arguments as an array of pointers (as scripts get them).
    let mut stack = [core::ptr::null_mut::<RValue>(); BUILTIN_ARGS_ON_STACK];
    let mut heap = Vec::new();
    let argv: *mut *mut RValue = if n <= BUILTIN_ARGS_ON_STACK {
        for (i, p) in stack.iter_mut().take(n).enumerate() {
            // SAFETY: `args` holds `argc` contiguous values (runner contract).
            *p = unsafe { args.add(i) };
        }
        stack.as_mut_ptr()
    } else {
        // SAFETY: as above.
        heap.extend((0..n).map(|i| unsafe { args.add(i) }));
        heap.as_mut_ptr()
    };
    let mut call = HookCall {
        self_inst,
        other_inst,
        result,
        argc: n as i32,
        argv,
        ret: result,
        site: N as u32,
        next: 0,
    };
    // SAFETY: `call` (and the pointer array) is live for the duration of the chain.
    unsafe { call_next(&raw mut call) };
}

unsafe extern "C-unwind" fn event_thunk<const N: usize>(
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
) {
    count_call(N);
    let mut call = HookCall {
        self_inst,
        other_inst,
        result: core::ptr::null_mut(),
        argc: 0,
        argv: core::ptr::null_mut(),
        ret: core::ptr::null_mut(),
        site: N as u32,
        next: 0,
    };
    // SAFETY: `call` is live for the duration of the chain.
    unsafe { call_next(&raw mut call) };
}

#[allow(clippy::similar_names)] // argc/argv are the conventional names
unsafe extern "C-unwind" fn script_thunk<const N: usize>(
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    result: *mut RValue,
    argc: i32,
    argv: *mut *mut RValue,
) -> *mut RValue {
    count_call(N);
    let mut call = HookCall {
        self_inst,
        other_inst,
        result,
        argc,
        argv,
        ret: result,
        site: N as u32,
        next: 0,
    };
    // SAFETY: `call` is live for the duration of the chain.
    unsafe { call_next(&raw mut call) };
    call.ret
}

// ---- GML error guard: the `ExecuteIt` detour and the runner state it restores ----

/// `bool ExecuteIt(CInstance* self, CInstance* other, CCode* code, RValue*
/// result, int flags)`; the result is in `al`, forwarded as the whole `rax`.
type ExecuteItFn = unsafe extern "C-unwind" fn(
    *mut CInstance,
    *mut CInstance,
    *mut c_void,
    *mut RValue,
    i32,
) -> u64;

/// `CInstance` fields the guard reads: `id` and `object_index` (the inner
/// `Perform_Event` passes `+0xC0` as the object; the runner's mouse-over code
/// reads `+0xBC` as the id).
const INSTANCE_ID: usize = 0xBC;
const INSTANCE_OBJECT: usize = 0xC0;

#[derive(Debug)]
struct Guard {
    sites: crate::guard::Sites,
    /// `ExecuteIt`'s trampoline when the detour is installed.
    execute_it: Option<usize>,
    /// The detour runs the event guard (else it is there for observers only).
    events: bool,
}

static GUARD: OnceLock<Guard> = OnceLock::new();

/// What the `ExecuteIt` detour does (independent switches).
#[derive(Clone, Copy, Debug, Default)]
pub struct DetourUse {
    /// Contain errors in marked instances' events (the event guard).
    pub events: bool,
    /// Call the event observers (`sites.roles` must be known).
    pub observers: bool,
    /// Measure the detour's cost and log it (`event_stats`).
    pub stats: bool,
}

/// Install the guard with `sites` (found by `guard::find_sites`): the call
/// guard always, the `ExecuteIt` detour when the event guard or observers need
/// it. Boot only (game code not running yet).
pub fn install_guard(sites: crate::guard::Sites, using: DetourUse) -> Result<(), String> {
    let mut execute_it = None;
    let mut detour = None;
    let events = using.events;
    if using.events || using.observers {
        // `sites.execute_it` is the runner's `ExecuteIt` (found by shape: the
        // single callee of the event executor wrapper, which calls `YYError`),
        // and `execute_it_hook` has its signature.
        let d = Detour::new(
            sites.execute_it,
            execute_it_hook as *const () as usize,
            Calls::Refuse,
        )
        .map_err(|e| format!("detour ExecuteIt: {e}"))?;
        execute_it = Some(d.trampoline());
        detour = Some(d);
    }
    GUARD
        .set(Guard {
            sites,
            execute_it,
            events,
        })
        .map_err(|_| "guard installed twice".to_owned())?;
    if using.stats && execute_it.is_some() {
        STATS_ON.store(true, Ordering::Relaxed);
    }
    if let Some(d) = detour {
        // SAFETY: game threads are not yet running GML (this runs in the entry
        // hook); the trampoline is published in `GUARD` above.
        unsafe { d.enable() }.map_err(|e| format!("enable ExecuteIt detour: {e}"))?;
    }
    Ok(())
}

/// Read `id` and `object_index` of a live instance (`(-1, -1)` for null).
fn instance_fields(inst: *mut CInstance) -> (i32, i32) {
    if inst.is_null() {
        return (-1, -1);
    }
    // SAFETY: `inst` is the live instance the runner passed to `ExecuteIt`;
    // both fields lie inside the `CInstance` object.
    unsafe {
        (
            inst.cast::<u8>()
                .add(INSTANCE_ID)
                .cast::<i32>()
                .read_unaligned(),
            inst.cast::<u8>()
                .add(INSTANCE_OBJECT)
                .cast::<i32>()
                .read_unaligned(),
        )
    }
}

/// The runner globals a call into game code can leave changed when an error
/// unwinds it: what the inner `Perform_Event` and the executor wrapper save
/// and restore around an event, and the GC root stack count (pushes whose pops
/// were skipped). YYC's own `catch` restores the GC root stack the same way.
struct Snapshot {
    dwords: [u32; 8],
    bytes: [u8; 8],
    gc_count: i32,
}

impl Snapshot {
    fn take(s: &crate::guard::Sites) -> Self {
        let mut snap = Snapshot {
            dwords: [0; 8],
            bytes: [0; 8],
            gc_count: 0,
        };
        // SAFETY: every address is a global in the game's writable `.data`
        // (the scan checks the section), valid for the process lifetime; read
        // on the game thread, which owns them.
        unsafe {
            for (v, &a) in snap.dwords.iter_mut().zip(&s.event_dwords) {
                *v = (a as *const u32).read_volatile();
            }
            for (v, &a) in snap.bytes.iter_mut().zip(&s.exec_bytes) {
                *v = (a as *const u8).read_volatile();
            }
            snap.gc_count = (s.gc_root_count as *const i32).read_volatile();
        }
        snap
    }

    fn restore(&self, s: &crate::guard::Sites) {
        // SAFETY: as in `take`; the values are the runner's own from before
        // the abandoned call, written back on the game thread.
        unsafe {
            for (v, &a) in self.dwords.iter().zip(&s.event_dwords) {
                (a as *mut u32).write_volatile(*v);
            }
            for (v, &a) in self.bytes.iter().zip(&s.exec_bytes) {
                (a as *mut u8).write_volatile(*v);
            }
            let count = s.gc_root_count as *mut i32;
            // Only drop entries pushed during the call; never grow the stack.
            if count.read_volatile() > self.gc_count {
                count.write_volatile(self.gc_count);
            }
        }
    }
}

// ---- Draw state an abandoned event leaves behind ----
//
// An error that abandons game code partway leaves whatever it had set for
// drawing. The runner checks one of these and ends the game on it: the
// surface target stack ("Unbalanced surface stack. You MUST use
// surface_reset_target() for each set.", checked after the event, at the next
// frame's check). The rest is not checked (the only other stack message, the
// GPU state stack's "state stack is empty", is a log line), but it would draw
// everything after it wrongly: the shader, the blend mode and its switch, the
// draw alpha and colour. Each is read before a guarded event (the surface
// target for every kind, the rest for Draw events) and put back after an
// abandoned one, through the runner's own builtins, resolved by name (none
// found: nothing restored).

/// GameMaker's Draw event kind (`ev_draw`: Draw, Draw GUI, their Begin and
/// End, Pre and Post Draw).
const EV_DRAW: i32 = 8;

/// Most `surface_reset_target` calls one restore makes.
const MAX_SURFACE_POPS: usize = 32;

/// The builtins of the draw state (routine addresses).
#[derive(Debug, Clone, Copy)]
struct DrawFns {
    surface_get: usize,
    surface_reset: usize,
    shader_current: usize,
    shader_reset: usize,
    shader_set: usize,
    blend_src: usize,
    blend_dest: usize,
    blend_set_ext: usize,
    /// The separate alpha factors (`gpu_set_blendmode_ext` sets them equal to
    /// the colour ones); `None` on a runner without them.
    sep_alpha: Option<SepAlpha>,
    blend_enable_get: usize,
    blend_enable_set: usize,
    alpha_get: usize,
    alpha_set: usize,
    colour_get: usize,
    colour_set: usize,
}

#[derive(Debug, Clone, Copy)]
struct SepAlpha {
    src_get: usize,
    dest_get: usize,
    set: usize,
}

static DRAW_FNS: OnceLock<Option<DrawFns>> = OnceLock::new();

/// The draw-state builtins, resolved once by name from the runner's
/// registrations; `None` (logged once) if any is missing.
fn draw_fns() -> Option<DrawFns> {
    *DRAW_FNS.get_or_init(|| {
        let r = |n: &str| crate::host::builtin_routine(n).ok();
        let fns = (|| {
            Some(DrawFns {
                surface_get: r("surface_get_target")?,
                surface_reset: r("surface_reset_target")?,
                shader_current: r("shader_current")?,
                shader_reset: r("shader_reset")?,
                shader_set: r("shader_set")?,
                blend_src: r("gpu_get_blendmode_src")?,
                blend_dest: r("gpu_get_blendmode_dest")?,
                blend_set_ext: r("gpu_set_blendmode_ext")?,
                sep_alpha: (|| {
                    Some(SepAlpha {
                        src_get: r("gpu_get_blendmode_srcalpha")?,
                        dest_get: r("gpu_get_blendmode_destalpha")?,
                        set: r("gpu_set_blendmode_ext_sepalpha")?,
                    })
                })(),
                blend_enable_get: r("gpu_get_blendenable")?,
                blend_enable_set: r("gpu_set_blendenable")?,
                alpha_get: r("draw_get_alpha")?,
                alpha_set: r("draw_set_alpha")?,
                colour_get: r("draw_get_colour")?,
                colour_set: r("draw_set_colour")?,
            })
        })();
        match fns {
            Some(f) => log::debug!(
                "GML error guard: draw state of abandoned events restored (surface target stack, shader, blend{}, alpha, colour)",
                if f.sep_alpha.is_some() {
                    " with its separate alpha factors"
                } else {
                    ""
                }
            ),
            None => log::warn!(
                "unable to find every draw-state builtin; draw state not restored after a contained error"
            ),
        }
        fns
    })
}

/// A value read from a getter: numbers, booleans and refs only (anything
/// owning is released and not kept).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Plain {
    payload: u64,
    kind: u32,
}

impl Plain {
    fn rvalue(self) -> RValue {
        RValue {
            payload: self.payload,
            flags: 0,
            kind: self.kind,
        }
    }

    /// `shader_current()` with no shader set: the real -1 (with one, a shader
    /// ref, kind 15).
    fn is_no_shader(self) -> bool {
        self.kind == ndml_sdk::abi::kind::REAL && f64::from_bits(self.payload) < 0.0
    }
}

/// Call a getter builtin (no arguments) directly, outside a guard frame.
///
/// # Safety
/// `routine` is one of the getters in [`DrawFns`]; game thread;
/// `self_inst`/`other_inst` are the instances of the running event or null.
unsafe fn get_plain(
    routine: usize,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
) -> Option<Plain> {
    let mut r = RValue::undefined();
    // SAFETY: caller contract; `r` owns nothing before the call. These getters
    // cannot raise a GML error when given no arguments: `surface_get_target`,
    // `draw_get_alpha` and `draw_get_colour` read a global and return; the
    // `gpu_get_blendmode_*` and `gpu_get_blendenable` routines raise
    // ("... shouldn't take any arguments") only when argc is not 0, then read
    // the render-state table (an index bounds check, no error); and
    // `shader_current` reads the current shader and returns -1 or its ref.
    unsafe {
        let f = std::mem::transmute::<usize, BuiltinFn>(routine);
        f(&raw mut r, self_inst, other_inst, 0, core::ptr::null_mut());
    }
    if r.is_owning() {
        // SAFETY: `r` is owned here, made by the call above.
        unsafe { crate::host::release_value(&raw mut r) };
        return None;
    }
    Some(Plain {
        payload: r.payload,
        kind: r.type_tag(),
    })
}

/// Call a builtin with `args` inside the call guard (a setter, or the
/// surface pop that may find the stack empty); false if it raised an error.
///
/// # Safety
/// `routine` is a builtin routine taking `args`; game thread.
unsafe fn call_set(
    routine: usize,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    args: &mut [RValue],
) -> bool {
    let mut result = RValue::undefined();
    let a = [
        (&raw mut result) as u64,
        self_inst as u64,
        other_inst as u64,
        args.len() as u64,
        args.as_mut_ptr() as u64,
        0,
    ];
    // SAFETY: caller contract; `args` hold plain values that live through the
    // call and `result` owns nothing.
    let ok = unsafe { guarded_call(routine, a) }.is_ok();
    if result.is_owning() {
        // SAFETY: the result is owned here.
        unsafe { crate::host::release_value(&raw mut result) };
    }
    ok
}

/// The look a Draw event started with.
#[derive(Debug, Clone, Copy)]
struct Look {
    shader: Option<Plain>,
    src: Option<Plain>,
    dest: Option<Plain>,
    /// The separate alpha factors (source, destination), when readable.
    alpha_factors: Option<(Plain, Plain)>,
    enable: Option<Plain>,
    alpha: Option<Plain>,
    colour: Option<Plain>,
}

/// The draw state when a guarded event (or script call) began.
#[derive(Debug, Clone, Copy)]
struct DrawState {
    surface: Option<Plain>,
    /// Draw events only.
    look: Option<Look>,
}

/// What a restore did, for the log.
#[derive(Debug, Default)]
struct Restored {
    done: Vec<String>,
    /// Setters that raised an error (inside the call guard).
    failed: Vec<String>,
}

impl Restored {
    fn note(&mut self, ok: bool, what: impl Into<String>) {
        if ok {
            self.done.push(what.into());
        } else {
            self.failed.push(what.into());
        }
    }
}

impl DrawState {
    /// Read the state (the surface target always; the rest for a Draw event).
    fn take(self_inst: *mut CInstance, other_inst: *mut CInstance, draw: bool) -> Option<Self> {
        let f = draw_fns()?;
        let t0 = STATS_ON.load(Ordering::Relaxed).then(tsc);
        // SAFETY: the routines are the named getters (see `get_plain`), called
        // on the game thread with the running event's instances.
        let get = |r| unsafe { get_plain(r, self_inst, other_inst) };
        let state = DrawState {
            surface: get(f.surface_get),
            look: draw.then(|| Look {
                shader: get(f.shader_current),
                src: get(f.blend_src),
                dest: get(f.blend_dest),
                alpha_factors: f
                    .sep_alpha
                    .and_then(|s| Some((get(s.src_get)?, get(s.dest_get)?))),
                enable: get(f.blend_enable_get),
                alpha: get(f.alpha_get),
                colour: get(f.colour_get),
            }),
        };
        if let Some(t0) = t0 {
            let reads = 1 + if draw {
                6 + 2 * u64::from(f.sep_alpha.is_some())
            } else {
                0
            };
            stats_draw(tsc().saturating_sub(t0), reads);
        }
        Some(state)
    }

    /// Put it back after an abandoned event.
    fn restore(&self, self_inst: *mut CInstance, other_inst: *mut CInstance) -> Restored {
        let mut out = Restored::default();
        let Some(f) = draw_fns() else {
            return out;
        };
        // SAFETY: as in `take`.
        let get = |r| unsafe { get_plain(r, self_inst, other_inst) };
        // SAFETY: the named setters with their own argument counts, on the game
        // thread; arguments are plain values copied from the getters.
        let set = |r, args: &mut [RValue]| unsafe { call_set(r, self_inst, other_inst, args) };
        if let Some(start) = self.surface {
            let mut pops = 0;
            let mut ok = true;
            while pops < MAX_SURFACE_POPS && get(f.surface_get).is_some_and(|now| now != start) {
                if !set(f.surface_reset, &mut []) {
                    ok = false;
                    break;
                }
                pops += 1;
            }
            if pops > 0 || !ok {
                out.note(ok, format!("{pops} surface target(s) reset"));
            }
        }
        let Some(l) = self.look else {
            return out;
        };
        if let Some(s) = l.shader
            && get(f.shader_current) != Some(s)
        {
            let ok = if s.is_no_shader() {
                set(f.shader_reset, &mut [])
            } else {
                set(f.shader_set, &mut [s.rvalue()])
            };
            out.note(ok, "shader");
        }
        if let (Some(s), Some(d)) = (l.src, l.dest) {
            let now = (get(f.blend_src), get(f.blend_dest));
            let now_alpha = f.sep_alpha.map(|sa| (get(sa.src_get), get(sa.dest_get)));
            let colour_changed = now != (Some(s), Some(d));
            let alpha_changed = match (l.alpha_factors, now_alpha) {
                (Some((sa, da)), Some(n)) => n != (Some(sa), Some(da)),
                _ => false,
            };
            if colour_changed || alpha_changed {
                // Separate alpha factors that differed from the colour ones
                // come back through the four-factor setter; otherwise the
                // two-factor one (which sets the alpha factors equal).
                let ok = match (l.alpha_factors, f.sep_alpha) {
                    (Some((sa, da)), Some(sep)) if (sa, da) != (s, d) => set(
                        sep.set,
                        &mut [s.rvalue(), d.rvalue(), sa.rvalue(), da.rvalue()],
                    ),
                    _ => set(f.blend_set_ext, &mut [s.rvalue(), d.rvalue()]),
                };
                out.note(ok, "blend mode");
            }
        }
        for (start, getter, setter, what) in [
            (
                l.enable,
                f.blend_enable_get,
                f.blend_enable_set,
                "blend switch",
            ),
            (l.alpha, f.alpha_get, f.alpha_set, "alpha"),
            (l.colour, f.colour_get, f.colour_set, "colour"),
        ] {
            if let Some(v) = start
                && get(getter) != Some(v)
            {
                let ok = set(setter, &mut [v.rvalue()]);
                out.note(ok, what);
            }
        }
        out
    }
}

/// Restores of draw state so far (the log is rate-limited).
static DRAW_RESTORES: AtomicU64 = AtomicU64::new(0);

fn log_draw_restore(what: &str, r: &Restored) {
    if r.done.is_empty() && r.failed.is_empty() {
        return;
    }
    let n = DRAW_RESTORES.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 20 || n.is_power_of_two() {
        let failed = if r.failed.is_empty() {
            String::new()
        } else {
            format!(
                "; FAILED (the setter raised an error): {}",
                r.failed.join(", ")
            )
        };
        log::warn!(
            "GML error guard: {what} left draw state changed; put back: {}{failed} (#{n})",
            if r.done.is_empty() {
                "nothing".to_owned()
            } else {
                r.done.join(", ")
            }
        );
    }
}

/// Whether the current event is a Draw event (false when the event globals
/// were not recognised).
fn in_draw_event(g: &Guard) -> bool {
    g.sites.roles.is_some_and(|r| read_event(&r).0 == EV_DRAW)
}

/// Whether the guard is installed (the call guard can run).
#[must_use]
pub fn guard_installed() -> bool {
    GUARD.get().is_some()
}

/// Run game function `target` with `args` inside a mod-call guard frame.
/// `Err(thrown)` when a GML error was raised inside (the runner state above is
/// restored); `Ok(rax)` otherwise. Without an installed guard it just calls.
///
/// # Safety
/// As `crate::win::guard_call`: `target` takes (up to) six integer
/// arguments and `args` are valid for it; game thread only.
pub unsafe fn guarded_call(target: usize, args: [u64; 6]) -> Result<u64, [u64; 2]> {
    type Six = unsafe extern "C-unwind" fn(u64, u64, u64, u64, u64, u64) -> u64;
    let Some(g) = GUARD.get() else {
        // SAFETY: caller contract.
        return Ok(unsafe {
            std::mem::transmute::<usize, Six>(target)(
                args[0], args[1], args[2], args[3], args[4], args[5],
            )
        });
    };
    let snap = Snapshot::take(&g.sites);
    let token = crate::guard::call_enter();
    // SAFETY: caller contract.
    let r = unsafe { crate::win::guard_call(target, args, crate::guard::decide_call, 0) };
    if r.is_err() {
        snap.restore(&g.sites);
    }
    crate::guard::call_leave(token);
    r
}

/// A draw-state restore waiting for its caller ([`guarded_script_call`]): the
/// caller reads the error first (the thrown value is a copy that owns
/// nothing, and the setters run game code), then calls [`Self::finish`].
#[derive(Debug)]
#[must_use = "finish() puts the draw state back"]
pub struct PendingDrawRestore {
    state: Option<DrawState>,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
}

impl PendingDrawRestore {
    /// Put the draw state back (game thread, right after the failed call).
    pub fn finish(self) {
        if let Some(d) = self.state {
            let r = d.restore(self.self_inst, self.other_inst);
            log_draw_restore("a mod's script call abandoned by an error", &r);
        }
    }
}

/// [`guarded_call`] for a compiled script (`args` as the script call's: self,
/// other, result, argc, argv): game code that can set draw state, so the
/// surface target (and in a Draw event the rest of `DrawState`) is put back
/// when an error abandons it, by the returned [`PendingDrawRestore`] once the
/// caller has read the error.
///
/// # Safety
/// As [`guarded_call`].
#[allow(clippy::result_large_err)] // a copy on return; no allocation on the call path
pub unsafe fn guarded_script_call(
    target: usize,
    args: [u64; 6],
) -> Result<u64, ([u64; 2], PendingDrawRestore)> {
    let (self_inst, other_inst) = (args[0] as *mut CInstance, args[1] as *mut CInstance);
    let draw = GUARD
        .get()
        .and_then(|g| DrawState::take(self_inst, other_inst, in_draw_event(g)));
    // SAFETY: caller contract.
    unsafe { guarded_call(target, args) }.map_err(|thrown| {
        (
            thrown,
            PendingDrawRestore {
                state: draw,
                self_inst,
                other_inst,
            },
        )
    })
}

unsafe extern "C-unwind" fn execute_it_hook(
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    code: *mut c_void,
    result: *mut RValue,
    flags: i32,
) -> u64 {
    let Some((g, tramp)) = GUARD.get().and_then(|g| Some((g, g.execute_it?))) else {
        return 1; // unreachable: the detour exists only with a trampoline
    };
    let stats = STATS_ON.load(Ordering::Relaxed);
    let t0 = if stats { tsc() } else { 0 };
    let observers = crate::guard::observers();
    // The current event, from the globals `Perform_Event` set just before
    // calling the executor: read only when something needs it.
    let event = if observers.is_empty() && !stats {
        None
    } else {
        g.sites.roles.map(|r| read_event(&r))
    };
    let mut seen = None;
    if let Some((kind, number, object)) = event
        && observers.iter().any(|o| o.matches(kind, number))
    {
        let (id, instance_object) = instance_fields(self_inst);
        let s = EventSeen {
            self_inst,
            other_inst,
            id,
            instance_object,
            object,
            kind,
            number,
            after: 0,
        };
        notify(observers, &s);
        seen = Some(s);
    }
    if stats {
        stats_enter(event.map(|e| e.0), t0);
    }
    let mut time = Timing::default();
    let ret = if g.events {
        // SAFETY: forwarding the runner's own arguments (caller contract).
        unsafe {
            guarded_event(
                g,
                tramp,
                [self_inst, other_inst],
                code,
                result,
                flags,
                stats.then_some(&mut time),
            )
        }
    } else {
        if stats {
            time.t1 = tsc();
        }
        // SAFETY: trampoline of `ExecuteIt`, forwarding the runner's arguments.
        let r = unsafe {
            let f = std::mem::transmute::<usize, ExecuteItFn>(tramp);
            f(self_inst, other_inst, code, result, flags)
        };
        if stats {
            time.t2 = tsc();
        }
        r
    };
    if let Some(mut s) = seen {
        s.after = 1;
        notify(observers, &s);
    }
    if stats {
        stats_leave(t0, &time, seen.is_some());
    }
    ret
}

/// Kind, number and object of the current event.
fn read_event(r: &crate::guard::EventRoles) -> (i32, i32, i32) {
    // SAFETY: the three are `int32` globals in the game's writable `.data`
    // (found by `event_roles`), valid for the process lifetime and written by
    // the game thread this runs on.
    unsafe {
        (
            (r.kind as *const i32).read_volatile(),
            (r.number as *const i32).read_volatile(),
            (r.object as *const i32).read_volatile(),
        )
    }
}

/// Call every observer whose filter matches `seen`.
fn notify(observers: &[crate::guard::Observer], seen: &EventSeen) {
    for o in observers {
        if o.matches(seen.kind, seen.number) {
            // SAFETY: the mod registered `cb` with `user` during init (mods
            // stay loaded for the process lifetime); `seen` lives for the call.
            unsafe { (o.cb)(o.user as *mut c_void, seen) };
        }
    }
}

/// Whether an event of kind `kind` of instance `id` (of `object`) is in a
/// mod's wider scope (`guard::set_scope`): an offspring, or its object.
fn scope_why(id: i32, object: i32, kind: i32) -> Option<crate::guard::Why> {
    if crate::guard::offspring_event(id, kind) {
        Some(crate::guard::Why::Offspring)
    } else if crate::guard::object_contained(object) && crate::guard::scope_kind(kind) {
        Some(crate::guard::Why::Object)
    } else {
        None
    }
}

/// The event guard's part of the detour: the event inside an SEH frame when
/// its instance is marked or it runs inside a guarded mod call.
///
/// # Safety
/// `tramp` is `ExecuteIt`'s trampoline and the rest the runner's arguments.
unsafe fn guarded_event(
    g: &Guard,
    tramp: usize,
    [self_inst, other_inst]: [*mut CInstance; 2],
    code: *mut c_void,
    result: *mut RValue,
    flags: i32,
    mut time: Option<&mut Timing>,
) -> u64 {
    let depth = crate::guard::event_enter();
    let (id, object) = instance_fields(self_inst);
    let tm = if time.is_some() { tsc() } else { 0 };
    let why = if crate::guard::is_marked(id) {
        Some(crate::guard::Why::Marked)
    } else if let Some(why) = crate::guard::any_scope()
        .then(|| {
            g.sites
                .roles
                .and_then(|r| scope_why(id, object, read_event(&r).0))
        })
        .flatten()
    {
        Some(why)
    } else if crate::guard::in_guarded_call() {
        Some(crate::guard::Why::InCall)
    } else {
        None
    };
    if let Some(t) = time.as_deref_mut() {
        t.t1 = tsc();
        t.marked = t.t1 - tm;
    }
    let ret = if let Some(why) = why {
        let snap = Snapshot::take(&g.sites);
        let draw = DrawState::take(self_inst, other_inst, in_draw_event(g));
        let contained = crate::guard::contained_enter();
        let args = [
            self_inst as u64,
            other_inst as u64,
            code as u64,
            result as u64,
            u64::from(flags as u32),
            0,
        ];
        // SAFETY: the trampoline runs the original `ExecuteIt` with the
        // runner's own arguments (five; the sixth slot is unused stack).
        let r = unsafe {
            crate::win::guard_call(tramp, args, crate::guard::decide_event, depth as usize)
        };
        crate::guard::contained_leave(contained);
        match r {
            Ok(r) => r,
            Err(thrown) => {
                snap.restore(&g.sites);
                // This event is the innermost again (the error unwound the rest).
                crate::guard::event_leave(depth + 1);
                // The error first: `thrown` is a copy that owns nothing, and
                // the draw-state setters below run game code.
                let text = crate::host::error_text(thrown);
                if let Some(d) = draw {
                    let r = d.restore(self_inst, other_inst);
                    log_draw_restore(&format!("an abandoned event of object {object}"), &r);
                }
                let line = text.trim().replace('\n', " | ");
                let expected = crate::guard::expecting();
                if expected {
                    crate::guard::log_expected(
                        object,
                        &format!("event of instance {id} (object {object}) abandoned"),
                        &line,
                    );
                } else if let Some(n) = crate::guard::log_due(object, &line) {
                    log::warn!(
                        "GML error contained (#{n} at this site): event of instance {id} (object {object}, {}) abandoned: {line}",
                        why.describe(),
                    );
                }
                crate::guard::push_report(crate::guard::Report {
                    caught: crate::guard::Caught::Event,
                    instance: id,
                    object,
                    why,
                    text,
                    expected,
                });
                1
            }
        }
    } else {
        // SAFETY: trampoline of `ExecuteIt`, forwarding the runner's arguments.
        unsafe {
            let f = std::mem::transmute::<usize, ExecuteItFn>(tramp);
            f(self_inst, other_inst, code, result, flags)
        }
    };
    if let Some(t) = time {
        t.t2 = tsc();
    }
    crate::guard::event_leave(depth);
    ret
}

// ---- detour cost (`event_stats` in ndml.toml; dev) ----

/// Measure the `ExecuteIt` detour: set at install, read per event.
static STATS_ON: AtomicBool = AtomicBool::new(false);
/// How often the cost line is logged.
const EVENT_STATS_PERIOD: Duration = Duration::from_secs(10);
/// Event kind of Draw events (the last phase of a frame) and of the first
/// phases of the next one (Alarm, Step: Begin Step is Step 1).
const KIND_ALARM: i32 = 2;
const KIND_STEP: i32 = 3;
const KIND_DRAW: i32 = 8;

fn tsc() -> u64 {
    // SAFETY: `rdtsc` is available on every x86-64 CPU; no memory access.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Timestamps of one event inside the detour: around the original, and the
/// marked check's share.
#[derive(Default)]
struct Timing {
    /// Just before the original runs.
    t1: u64,
    /// Just after it returned.
    t2: u64,
    /// Cycles spent in the marked lookup.
    marked: u64,
}

/// Per-frame and per-window sums (game thread).
#[derive(Default)]
struct EventStats {
    prev_kind: i32,
    frame_start: u64,
    f_events: u64,
    f_over: u64,
    f_marked: u64,
    // window
    window_tsc: u64,
    window_at: Option<Instant>,
    frames: u64,
    events: u64,
    max_events: u64,
    over: u64,
    max_over: u64,
    marked: u64,
    frame_cycles: u64,
    max_frame: u64,
    max_marked: usize,
    observed: u64,
    /// The guard's draw-state reads (`DrawState::take`): this frame's
    /// cycles and getter calls, the window's, the worst frame's cycles.
    f_draw: u64,
    f_draw_reads: u64,
    draw: u64,
    draw_reads: u64,
    max_draw: u64,
}

thread_local! {
    static EVENT_STATS: std::cell::RefCell<EventStats> = std::cell::RefCell::default();
}

/// An event enters the detour at `now`: a Step/Alarm event right after a Draw
/// event starts a new frame.
fn stats_enter(kind: Option<i32>, now: u64) {
    let Some(kind) = kind else { return };
    EVENT_STATS.with_borrow_mut(|s| {
        if s.prev_kind == KIND_DRAW && (kind == KIND_STEP || kind == KIND_ALARM) {
            s.end_frame(now);
        }
        s.prev_kind = kind;
    });
}

/// The guard read the draw state: `cycles` for `reads` getter calls (inside
/// the event's timed span, which the detour cost leaves out, so counted
/// apart).
fn stats_draw(cycles: u64, reads: u64) {
    EVENT_STATS.with_borrow_mut(|s| {
        s.f_draw += cycles;
        s.f_draw_reads += reads;
    });
}

/// The event left the detour: add its cost (the detour's own time, the
/// original's excluded).
fn stats_leave(t0: u64, time: &Timing, observed: bool) {
    let t3 = tsc();
    let over = time.t1.saturating_sub(t0) + t3.saturating_sub(time.t2);
    EVENT_STATS.with_borrow_mut(|s| {
        s.f_events += 1;
        s.f_over += over;
        s.f_marked += time.marked;
        s.observed += u64::from(observed);
    });
}

impl EventStats {
    #[allow(clippy::cast_precision_loss)] // statistics
    fn end_frame(&mut self, now: u64) {
        if self.frame_start != 0 {
            let cycles = now.saturating_sub(self.frame_start);
            self.frames += 1;
            self.events += self.f_events;
            self.max_events = self.max_events.max(self.f_events);
            self.over += self.f_over;
            self.max_over = self.max_over.max(self.f_over);
            self.marked += self.f_marked;
            self.frame_cycles += cycles;
            self.max_frame = self.max_frame.max(cycles);
            self.max_marked = self.max_marked.max(crate::guard::marked_count());
            self.draw += self.f_draw;
            self.draw_reads += self.f_draw_reads;
            self.max_draw = self.max_draw.max(self.f_draw);
        }
        self.frame_start = now;
        (self.f_events, self.f_over, self.f_marked) = (0, 0, 0);
        (self.f_draw, self.f_draw_reads) = (0, 0);
        let at = Instant::now();
        let Some(since) = self.window_at else {
            (self.window_at, self.window_tsc) = (Some(at), now);
            return;
        };
        let elapsed = at.duration_since(since);
        if elapsed < EVENT_STATS_PERIOD || self.frames == 0 {
            return;
        }
        // TSC ticks per microsecond over the window (no calibration needed).
        let per_us = now.saturating_sub(self.window_tsc) as f64 / elapsed.as_micros().max(1) as f64;
        let us = |c: u64| c as f64 / per_us;
        let n = self.frames as f64;
        let offspring = crate::guard::offspring_counts();
        log::info!(
            "event detour cost, last {:.0}s: {} frames, events/frame mean {:.0} max {}, detour µs/frame mean {:.1} worst {:.1} (marked and scope check mean {:.1}), ns/event {:.0}, frame ms mean {:.2} worst {:.2}, marked up to {}, offspring {} (made {}), observed events {}; draw-state reads/frame mean {:.0}, µs/frame mean {:.1} worst {:.1}, ns/read {:.0}",
            elapsed.as_secs_f64(),
            self.frames,
            self.events as f64 / n,
            self.max_events,
            us(self.over) / n,
            us(self.max_over),
            us(self.marked) / n,
            us(self.over) * 1000.0 / self.events.max(1) as f64,
            us(self.frame_cycles) / n / 1000.0,
            us(self.max_frame) / 1000.0,
            self.max_marked,
            offspring.0,
            offspring.1,
            self.observed,
            self.draw_reads as f64 / n,
            us(self.draw) / n,
            us(self.max_draw),
            us(self.draw) * 1000.0 / self.draw_reads.max(1) as f64,
        );
        *self = EventStats {
            prev_kind: self.prev_kind,
            frame_start: self.frame_start,
            window_tsc: now,
            window_at: Some(at),
            ..EventStats::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("gml_Object_oWidget_Step_0"), Some(Kind::Event));
        assert_eq!(Kind::of("gml_Script_widgetPull"), Some(Kind::Script));
        assert_eq!(Kind::of("gml_GlobalScript_widgetPull"), None);
        assert_eq!(Kind::of("builtin:instance_copy"), Some(Kind::Builtin));
        assert_eq!(Kind::of("instance_copy"), None);
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("gml_Object_oThing_Step_0"), "oThing_Step_0");
        assert_eq!(short_name("gml_Script_widgetPull"), "widgetPull()");
    }

    use crate::guard::{add_observer, freeze_observers, observers, observers_registered};

    unsafe extern "C-unwind" fn seen_nothing(
        _: *mut core::ffi::c_void,
        _: *const ndml_sdk::abi::EventSeen,
    ) {
    }

    fn observer(kind: i32, number: i32) -> crate::guard::Observer {
        crate::guard::Observer {
            kind,
            number,
            cb: seen_nothing,
            user: 0,
            mod_id: 0,
        }
    }

    /// A failed mod's event observers go; another mod's stay.
    #[test]
    fn a_failed_mods_observers_are_dropped() {
        let o = |mod_id| crate::guard::Observer {
            mod_id,
            ..observer(-1, -1)
        };
        let mut list = vec![o(1), o(2), o(1), o(3)];
        assert_eq!(crate::guard::without_mod(&mut list, 1), 2);
        assert_eq!(
            list.iter().map(|o| o.mod_id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(crate::guard::without_mod(&mut list, 9), 0);
    }

    #[test]
    fn observer_filters() {
        let user0 = observer(7, 10);
        assert!(user0.matches(7, 10));
        assert!(!user0.matches(7, 11) && !user0.matches(3, 10));
        assert!(observer(-1, 10).matches(3, 10));
        assert!(observer(7, -1).matches(7, 0) && !observer(7, -1).matches(8, 0));
    }

    /// The only test touching the global observer list.
    #[test]
    fn observers_register_until_frozen() {
        assert!(observers().is_empty());
        for i in 0..crate::guard::MAX_OBSERVERS {
            assert_eq!(add_observer(observer(7, 10)), Status::Ok, "{i}");
        }
        assert!(observers_registered());
        assert_eq!(add_observer(observer(7, 10)), Status::Exhausted);
        // Nothing runs before boot freezes the list.
        assert!(observers().is_empty());
        assert_eq!(freeze_observers(true), crate::guard::MAX_OBSERVERS);
        assert_eq!(observers().len(), crate::guard::MAX_OBSERVERS);
        assert_eq!(add_observer(observer(7, 10)), Status::HooksFrozen);
    }

    unsafe extern "C-unwind" fn noop(_: *mut HookCall, _: *mut c_void) {}

    fn link(mod_id: u32) -> Link {
        Link {
            cb: noop,
            user: core::ptr::null_mut(),
            mod_id,
        }
    }

    #[test]
    fn registry_fills_to_cap_then_refuses() {
        let mut reg = Registry::default();
        for i in 0..MAX_SITES {
            let (name, kind) = match i % 3 {
                0 => (format!("gml_Object_o{i}_Step_0"), Kind::Event),
                1 => (format!("gml_Script_s{i}"), Kind::Script),
                _ => (format!("builtin:b{i}"), Kind::Builtin),
            };
            let status = reg.add(&name, kind, 0x1000 + i, link(1));
            assert_eq!(status, Status::Ok, "site {i}");
        }
        assert_eq!(reg.sites.len(), MAX_SITES);
        // One past the cap is refused, whichever kind it is: the cap is shared.
        let past = 0x1000 + MAX_SITES;
        let status = reg.add("gml_Script_more", Kind::Script, past, link(1));
        assert_eq!(status, Status::Exhausted);
        let status = reg.add("gml_Object_more_Step_0", Kind::Event, past, link(1));
        assert_eq!(status, Status::Exhausted);
        let status = reg.add("builtin:more", Kind::Builtin, past, link(1));
        assert_eq!(status, Status::Exhausted);
        assert_eq!(reg.sites.len(), MAX_SITES);
        // Chaining onto an existing site still works at the cap.
        let status = reg.add("gml_Script_s1", Kind::Script, 0x1001, link(2));
        assert_eq!(status, Status::Ok);
        assert_eq!(reg.sites[1].chain.len(), 2);
        // Every site index has a thunk of each kind.
        assert_eq!(EVENT_THUNKS.len(), MAX_SITES);
        assert_eq!(SCRIPT_THUNKS.len(), MAX_SITES);
        assert_eq!(BUILTIN_THUNKS.len(), MAX_SITES);
    }

    /// A mod whose init fails after registering hooks: its links are dropped
    /// before `install` (which publishes every site's chain for `call_next`);
    /// another mod's link on the same site stays.
    #[test]
    fn a_failed_mods_hooks_are_dropped_before_install() {
        let mut reg = Registry::default();
        assert_eq!(
            reg.add("gml_Script_a", Kind::Script, 0x10, link(1)),
            Status::Ok
        );
        assert_eq!(
            reg.add("gml_Script_a", Kind::Script, 0x10, link(2)),
            Status::Ok
        );
        assert_eq!(
            reg.add("gml_Script_b", Kind::Script, 0x20, link(2)),
            Status::Ok
        );
        let of = |reg: &Registry, m: u32| {
            (reg.sites.iter())
                .flat_map(|s| s.chain.iter())
                .filter(|l| l.mod_id == m)
                .count()
        };
        assert_eq!(of(&reg, 2), 2, "registered: what install would publish");
        assert_eq!(reg.drop_mod(2), 2);
        assert_eq!(of(&reg, 2), 0);
        assert_eq!(reg.sites.len(), 1, "the site only mod 2 had is gone");
        assert_eq!((reg.sites[0].target, of(&reg, 1)), (0x10, 1));
        reg.frozen = true;
        assert_eq!(reg.drop_mod(1), 0, "nothing changes once frozen");
    }

    #[test]
    fn frozen_registry_refuses() {
        let mut reg = Registry {
            frozen: true,
            ..Registry::default()
        };
        let status = reg.add("gml_Script_x", Kind::Script, 1, link(1));
        assert_eq!(status, Status::HooksFrozen);
        assert!(reg.sites.is_empty());
    }

    #[test]
    fn thunk_tables_are_distinct() {
        assert_ne!(EVENT_THUNKS[0] as usize, EVENT_THUNKS[1] as usize);
        assert_ne!(
            EVENT_THUNKS[0] as usize,
            EVENT_THUNKS[MAX_SITES - 1] as usize
        );
        assert_ne!(
            SCRIPT_THUNKS[0] as usize,
            SCRIPT_THUNKS[MAX_SITES - 1] as usize
        );
    }

    extern "C" fn seven(_: u32) -> u32 {
        7
    }

    type Code = extern "C" fn(u32) -> u32;

    /// Write `code` into fresh code memory near this test binary.
    fn place(code: &[u8]) -> usize {
        let at = win::alloc_code_near(seven as *const () as usize, 64, detour::NEAR as usize)
            .expect("code memory");
        // SAFETY: fresh code memory nothing runs yet.
        unsafe { win::write_code(at, code) }.expect("write");
        at
    }

    fn run(at: usize, arg: u32) -> u32 {
        // SAFETY: `at` holds a complete function of this signature (the
        // tests' own machine code, or a trampoline built for one).
        let f: Code = unsafe { std::mem::transmute(at) };
        f(arg)
    }

    /// `cmp qword [rip+X], 0` with the immediate after the displacement, here
    /// reading a slot of its own. Through the trampoline it reads the same
    /// slot: the relocated displacement goes at its decoded place and the
    /// immediate stays as it was.
    #[test]
    fn a_detoured_rip_compare_runs_through_its_trampoline() {
        let mut code = vec![
            0x48, 0x83, 0xEC, 0x28, // sub rsp, 28h
            0x48, 0x83, 0x3D, 0x1C, 0, 0, 0, 0x00, // cmp qword [rip+1Ch] (slot at 40), 0
            0x74, 0x0A, // je +10 (to 24)
            0xB8, 1, 0, 0, 0, // mov eax, 1
            0x48, 0x83, 0xC4, 0x28, // add rsp, 28h
            0xC3, // ret
            0xB8, 2, 0, 0, 0, // 24: mov eax, 2
            0x48, 0x83, 0xC4, 0x28, // add rsp, 28h
            0xC3, // ret
        ];
        code.resize(40, 0xCC);
        code.extend_from_slice(&0u64.to_le_bytes()); // the slot
        let f = place(&code);
        assert_eq!(run(f, 0), 2);
        let d = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("detour");
        assert_eq!(d.plan().copied, 12);
        assert_eq!(d.plan().relocated(), "rip+0x4");
        // SAFETY: nothing else runs this test function.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 0), 7, "the patch goes to the replacement");
        assert_eq!(run(d.trampoline(), 0), 2, "slot 0: the original's je taken");
        // SAFETY: the slot is data in the test's own code memory.
        unsafe { win::write_code(f + 40, &5u64.to_le_bytes()) }.expect("write slot");
        assert_eq!(
            run(d.trampoline(), 0),
            1,
            "slot 5: the original falls through"
        );
    }

    /// `jcc rel8` in the copied bytes, its target past them: relocated to a
    /// `jcc rel32` into the original.
    #[test]
    fn a_detoured_short_jcc_runs_through_its_trampoline() {
        let code = [
            0x85, 0xC9, // test ecx, ecx
            0x74, 0x06, // je +6 (to 10)
            0xB8, 1, 0, 0, 0,    // mov eax, 1
            0xC3, // ret
            0xB8, 2, 0, 0, 0,    // 10: mov eax, 2
            0xC3, // ret
        ];
        let f = place(&code);
        let d = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("detour");
        assert_eq!(d.plan().copied, 9);
        assert_eq!(d.plan().relocated(), "jcc+0x2");
        // SAFETY: nothing else runs this test function.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 0), 7);
        assert_eq!(run(d.trampoline(), 0), 2);
        assert_eq!(run(d.trampoline(), 1), 1);
    }

    /// A function the detour cannot move is refused and left unchanged.
    #[test]
    fn a_refused_detour_leaves_the_function_alone() {
        let code = [
            0xE2, 0x02, // loop +2
            0x31, 0xC0, // xor eax, eax
            0xB8, 3, 0, 0, 0,    // mov eax, 3
            0xC3, // ret
        ];
        let f = place(&code);
        let e = Detour::new(f, seven as *const () as usize, Calls::Refuse).unwrap_err();
        assert!(
            e.to_string()
                .starts_with("its first bytes cannot be moved (at +0x0: loop or jrcxz"),
            "{e}"
        );
        assert_eq!(win::read_code(f, code.len()).unwrap(), code);
        // Registration reports the same reason.
        let r = detour_plan(f, Calls::Refuse).unwrap_err();
        assert!(matches!(r, DetourError::Refused { .. }));
    }

    extern "C" fn eight(_: u32) -> u32 {
        8
    }

    /// `test ecx, ecx; je +6; mov eax, 1; ret; mov eax, 2; ret`.
    const JCC_FN: [u8; 16] = [
        0x85, 0xC9, 0x74, 0x06, 0xB8, 1, 0, 0, 0, 0xC3, 0xB8, 2, 0, 0, 0, 0xC3,
    ];

    /// The CRT entry point's shape: `sub rsp, 28h; call helper; add rsp, 28h;
    /// add eax, 1; ret`, the helper (`mov eax, 40; ret`) at +32.
    fn entry_shape() -> Vec<u8> {
        let mut code = vec![
            0x48, 0x83, 0xEC, 0x28, // sub rsp, 28h
            0xE8, 0x17, 0, 0, 0, // call +23 (to 32)
            0x48, 0x83, 0xC4, 0x28, // add rsp, 28h
            0x83, 0xC0, 0x01, // add eax, 1
            0xC3, // ret
        ];
        code.resize(32, 0xCC);
        code.extend_from_slice(&[0xB8, 40, 0, 0, 0, 0xC3]);
        code
    }

    /// A function whose first bytes are already a hook's `jmp rel32` (this
    /// loader's own patch; another injector's looks the same) is detoured
    /// again: the second hook runs first, its trampoline reaches the first
    /// hook, the first hook's trampoline the original.
    #[test]
    fn a_detoured_function_is_detoured_again() {
        let f = place(&JCC_FN);
        let first = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("first");
        // SAFETY: nothing else runs this test function.
        unsafe { first.enable() }.expect("enable");
        let second = Detour::new(f, eight as *const () as usize, Calls::Refuse).expect("second");
        assert_eq!(second.plan().copied, 5);
        assert_eq!(second.plan().relocated(), "jmp+0x0");
        // SAFETY: as above.
        unsafe { second.enable() }.expect("enable");
        assert_eq!(run(f, 0), 8);
        assert_eq!(run(second.trampoline(), 0), 7);
        assert_eq!(run(first.trampoline(), 0), 2);
        assert_eq!(run(first.trampoline(), 1), 1);
    }

    /// The entry point's call, relocated (`Calls::Relocate`) and run.
    #[test]
    fn a_relocated_call_runs_through_its_trampoline() {
        let f = place(&entry_shape());
        assert_eq!(run(f, 0), 41);
        assert!(Detour::new(f, seven as *const () as usize, Calls::Refuse).is_err());
        let d = Detour::new(f, seven as *const () as usize, Calls::Relocate).expect("detour");
        assert_eq!(d.plan().copied, 9);
        // SAFETY: nothing else runs this test function.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 0), 7);
        assert_eq!(run(d.trampoline(), 0), 41);
    }

    /// An entry point that starts `call $+5; pop` (a wrapper finding its own
    /// address) is refused even with calls allowed: the entry hook then
    /// takes its "running unmodded" path. The function is left as it was.
    #[test]
    fn a_call_to_the_next_instruction_is_refused_at_the_entry_point() {
        let code = [
            0xE8, 0, 0, 0, 0,    // call $+5
            0x58, // pop rax
            0x31, 0xC0, // xor eax, eax
            0xC3, // ret
        ];
        let f = place(&code);
        let e = Detour::new(f, seven as *const () as usize, Calls::Relocate).unwrap_err();
        assert!(
            e.to_string().starts_with(
                "its first bytes cannot be moved (at +0x0: a call to the copied bytes"
            ),
            "{e}"
        );
        assert_eq!(win::read_code(f, code.len()).unwrap(), code);
    }

    /// `jmp rel8` over padding, `jmp rel32` (a tail call) and a 3-byte
    /// function over padding: each run through its trampoline.
    #[test]
    fn short_functions_run_through_their_trampolines() {
        // jmp +3; int3 x3; mov eax, 9; ret
        let f = place(&[0xEB, 0x03, 0xCC, 0xCC, 0xCC, 0xB8, 9, 0, 0, 0, 0xC3]);
        let d = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("jmp rel8");
        assert_eq!(d.plan().copied, 2);
        // SAFETY: nothing else runs this test function.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 0), 7);
        assert_eq!(run(d.trampoline(), 0), 9);
        // jmp +11 (to 16); padding; 16: mov eax, 11; ret
        let mut code = vec![0xE9, 0x0B, 0, 0, 0];
        code.resize(16, 0xCC);
        code.extend_from_slice(&[0xB8, 11, 0, 0, 0, 0xC3]);
        let f = place(&code);
        let d = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("jmp rel32");
        // SAFETY: as above.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 0), 7);
        assert_eq!(run(d.trampoline(), 0), 11);
        // xor eax, eax; ret; int3 x5 (3 bytes of function, the rest padding)
        let f = place(&[0x31, 0xC0, 0xC3, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC]);
        let d = Detour::new(f, seven as *const () as usize, Calls::Refuse).expect("short ret");
        assert_eq!(d.plan().copied, 3);
        // SAFETY: as above.
        unsafe { d.enable() }.expect("enable");
        assert_eq!(run(f, 5), 7);
        assert_eq!(run(d.trampoline(), 5), 0);
    }

    /// The forms `emit` writes when the trampoline is out of `rel32` reach of
    /// what the copied bytes address (an inverted `jcc` over `jmp [rip+0]`,
    /// the absolute jump back, `call [rip+2]; jmp +8`): built far from the
    /// function and run there.
    #[test]
    fn far_trampolines_run() {
        for (code, calls, expect) in [
            (JCC_FN.to_vec(), Calls::Refuse, [2, 1]),
            (entry_shape(), Calls::Relocate, [41, 41]),
        ] {
            let f = place(&code);
            let bytes = win::read_code(f, detour::READ_LEN).expect("read");
            let plan = detour::plan(&bytes, f as u64, calls).expect("plan");
            // Some far address with free memory near it (a fixed one can
            // fall inside a large reservation of the process).
            let far = [6usize, 12, 24, 48, 96, 200, 1000]
                .into_iter()
                .flat_map(|g| [f.checked_sub(g << 30), f.checked_add(g << 30)])
                .flatten()
                .find_map(|t| win::alloc_code_near(t, 128, detour::NEAR as usize))
                .expect("far code memory");
            assert!(far.abs_diff(f) > 1 << 32);
            let t = detour::emit(&plan, &bytes, f as u64, far as u64).expect("emit");
            assert!(t.len() <= detour::TRAMPOLINE_MAX);
            // SAFETY: fresh code memory nothing runs yet.
            unsafe { win::write_code(far, &t) }.expect("write");
            assert_eq!(run(far, 0), expect[0]);
            assert_eq!(run(far, 1), expect[1]);
        }
    }

    /// The documented limitation (Limitations in `docs/how-it-works.md`):
    /// a branch from later in the function back into its first bytes is not
    /// looked for. `xor eax, eax; L: add eax, ecx; dec ecx; jnz L; ret` sums
    /// 1..=n with `L` at +2, inside the patch: the planner accepts it (not
    /// run through a detour: the jump back would execute the patch's
    /// displacement as code).
    #[test]
    fn a_loop_back_into_the_patch_is_accepted_as_documented() {
        let code = [
            0x31, 0xC0, // xor eax, eax
            0x01, 0xC8, // 2: add eax, ecx
            0xFF, 0xC9, // dec ecx
            0x75, 0xFA, // jnz 2
            0xC3, // ret
        ];
        let f = place(&code);
        assert_eq!(run(f, 4), 10);
        let bytes = win::read_code(f, detour::READ_LEN).expect("read");
        let plan = detour::plan(&bytes, f as u64, Calls::Refuse).expect("accepted");
        assert_eq!(plan.copied, 6);
    }

    fn site(name: &str, target: usize) -> Site {
        Site {
            name: name.to_owned(),
            kind: Kind::Script,
            target,
            chain: vec![link(1)],
            trampoline: 0,
        }
    }

    /// `install`'s per-site step: a site whose detour cannot be built (here
    /// one at an address that is not code) is left out, the others are
    /// built and get their trampolines.
    #[test]
    fn a_site_that_cannot_be_detoured_is_left_out_and_the_others_built() {
        static NOT_CODE: [u8; 64] = [0x90; 64];
        let good = place(&JCC_FN);
        let refused = place(&[0xE2, 0x02, 0x31, 0xC0, 0xB8, 3, 0, 0, 0, 0xC3]);
        let mut sites = vec![
            site("gml_Script_good", good),
            site("gml_Script_data", NOT_CODE.as_ptr() as usize),
            site("gml_Script_loop", refused),
        ];
        let built = build_detours(&mut sites, |_, _| seven as *const () as usize);
        assert_eq!(built.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0]);
        assert_eq!(sites[0].trampoline, built[0].1.trampoline());
        assert_eq!((sites[1].trampoline, sites[2].trampoline), (0, 0));
        // Nothing enabled: the good function still runs as it is.
        assert_eq!(run(good, 0), 2);
    }

    /// A write that cannot be made fails before any byte is written.
    #[test]
    fn a_failed_code_write_is_reported() {
        // SAFETY: an address in the never-mapped first page: VirtualProtect
        // fails and the copy is never reached.
        let e = unsafe { win::write_code(0x10, &[0x90]) }.unwrap_err();
        assert!(e.starts_with("VirtualProtect failed"), "{e}");
    }

    /// Registration refuses a function the detour cannot move, with the
    /// status the mod gets and the reason for the log.
    #[test]
    fn registration_refuses_an_unmovable_function() {
        let f = place(&[0xE2, 0x02, 0x31, 0xC0, 0xB8, 3, 0, 0, 0, 0xC3]);
        let mut index = crate::yyc::RunnerIndex::default();
        index.functions.insert("gml_Script_loops".to_owned(), f);
        let r = register(&index, 1, "gml_Script_loops", noop, core::ptr::null_mut()).unwrap_err();
        assert_eq!(r.status, Status::Unsupported);
        assert!(r.why.unwrap().contains("loop or jrcxz"));
    }
}
