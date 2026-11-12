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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ndml_sdk::abi::{CInstance, HookCall, HookCallback, RValue, Status};
use retour::RawDetour;
use seq_macro::seq;

/// Maximum number of distinct hooked functions per kind.
pub const MAX_SITES: usize = 128;

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
    if TOTAL_CALLS.fetch_add(1, Ordering::Relaxed) % STATS_CHECK_EVERY == 0 {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `void gml_Object_<obj>_<Event>_<n>(CInstance* self, CInstance* other)`
    Event,
    /// `RValue* gml_Script_<name>(self, other, RValue* result, int argc, RValue** argv)`
    Script,
}

impl Kind {
    fn of(name: &str) -> Option<Self> {
        if name.starts_with("gml_Object_") {
            Some(Self::Event)
        } else if name.starts_with("gml_Script_") {
            Some(Self::Script)
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

// SAFETY: callback data pointers come from the SDK, which requires `Send + Sync`
// hook closures; sites are immutable once published.
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
/// (= mod load order): the first registered callback is outermost.
pub fn register(
    index: &crate::yyc::RunnerIndex,
    mod_id: u32,
    name: &str,
    cb: HookCallback,
    user: *mut c_void,
) -> Status {
    let Some(kind) = Kind::of(name) else {
        return Status::Unsupported;
    };
    let Some(&target) = index.functions.get(name) else {
        return Status::NotFound;
    };
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    if reg.frozen {
        return Status::HooksFrozen;
    }
    let link = Link { cb, user, mod_id };
    if let Some(site) = reg.sites.iter_mut().find(|s| s.target == target) {
        site.chain.push(link);
        return Status::Ok;
    }
    if reg.sites.len() >= MAX_SITES {
        return Status::Exhausted;
    }
    reg.sites.push(Site {
        name: name.to_owned(),
        kind,
        target,
        chain: vec![link],
        trampoline: 0,
    });
    Status::Ok
}

seq!(N in 0..128 {
    static EVENT_THUNKS: [EventFn; MAX_SITES] = [#(event_thunk::<N>,)*];
    static SCRIPT_THUNKS: [ScriptFn; MAX_SITES] = [#(script_thunk::<N>,)*];
});

/// Freeze registration and enable all detours. Returns the number of sites.
pub fn install() -> Result<usize, String> {
    let mut reg = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    reg.frozen = true;
    let mut sites = std::mem::take(&mut reg.sites);
    drop(reg);

    let mut detours = Vec::with_capacity(sites.len());
    for (i, site) in sites.iter_mut().enumerate() {
        let thunk = match site.kind {
            Kind::Event => EVENT_THUNKS[i] as *const (),
            Kind::Script => SCRIPT_THUNKS[i] as *const (),
        };
        // SAFETY: `target` is the entry of a compiled GML function found in the
        // runner table, and `thunk` has the matching signature.
        let detour = unsafe { RawDetour::new(site.target as *const (), thunk) }
            .map_err(|e| format!("detour {}: {e}", site.name))?;
        site.trampoline = std::ptr::from_ref(detour.trampoline()) as usize;
        detours.push(detour);
    }
    let count = sites.len();
    for site in &sites {
        let mods: Vec<u32> = site.chain.iter().map(|l| l.mod_id).collect();
        log::debug!(
            "hook site {} @ {:#x} ({:?}) mods {mods:?}",
            site.name,
            site.target,
            site.kind
        );
    }
    // Publish before enabling: a thunk may run the instant its detour is live.
    SITES
        .set(sites.into_boxed_slice())
        .map_err(|_| "hooks installed twice".to_owned())?;
    for d in &detours {
        // SAFETY: game threads are not yet running GML (this runs inside the
        // entry-point detour).
        unsafe { d.enable() }.map_err(|e| format!("enable detour: {e}"))?;
    }
    // Detours must live forever; the game calls through them until exit.
    std::mem::forget(detours);
    Ok(count)
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
        // SAFETY: the callback and its data pointer were registered together by
        // the SDK.
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
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(Kind::of("gml_Object_oBounty_Step_0"), Some(Kind::Event));
        assert_eq!(Kind::of("gml_Script_powerupAttract"), Some(Kind::Script));
        assert_eq!(Kind::of("gml_GlobalScript_powerupAttract"), None);
    }

    #[test]
    fn short_names() {
        assert_eq!(short_name("gml_Object_oPlayer_Step_0"), "oPlayer_Step_0");
        assert_eq!(short_name("gml_Script_powerupAttract"), "powerupAttract()");
    }

    #[test]
    fn thunk_tables_are_distinct() {
        assert_ne!(EVENT_THUNKS[0] as usize, EVENT_THUNKS[1] as usize);
        assert_ne!(
            SCRIPT_THUNKS[0] as usize,
            SCRIPT_THUNKS[MAX_SITES - 1] as usize
        );
    }
}
