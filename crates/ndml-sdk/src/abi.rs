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

//! The C ABI between the loader (`version.dll`) and mod DLLs.
//!
//! Everything here is `#[repr(C)]` and must stay layout-stable within an
//! [`ABI_VERSION`]. Additive changes go at the **end** of [`HostApi`] (mods check
//! `struct_size`); anything else bumps [`ABI_VERSION`]. See `docs/mod-api.md`.
//!
//! Pure data definitions: no unsafe code here (raw pointers are just fields).

#![forbid(unsafe_code)]

use core::ffi::c_void;

/// Bumped on any breaking change to the types in this module.
pub const ABI_VERSION: u32 = 1;

/// Name of the symbol every mod DLL exports (see [`crate::export_mod!`]).
pub const MOD_ENTRY_SYMBOL: &[u8] = b"ndml_mod_main\0";
/// Name of the symbol reporting the ABI version a mod was built against.
pub const MOD_ABI_SYMBOL: &[u8] = b"ndml_mod_abi_version\0";

/// A borrowed UTF-8 string crossing the DLL boundary. Not NUL-terminated.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct StrRef {
    pub ptr: *const u8,
    pub len: usize,
}

impl StrRef {
    #[must_use]
    pub const fn new(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }
    // `as_str` (unsafe) lives in `crate::ffi` to keep this module unsafe-free.
}

/// GameMaker's dynamically typed value (`RValue`), 16 bytes.
///
/// Layout (YYC runner, as used by Nova Drift): 8-byte payload, 4-byte flags,
/// 4-byte kind. Only the low 24 bits of `kind` are the type tag.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RValue {
    /// `f64` bits, `i32`/`i64`, or a pointer, depending on `kind`.
    pub payload: u64,
    pub flags: u32,
    pub kind: u32,
}

/// Type tags for [`RValue::kind`] (low 24 bits).
pub mod kind {
    pub const REAL: u32 = 0;
    pub const STRING: u32 = 1;
    pub const ARRAY: u32 = 2;
    pub const PTR: u32 = 3;
    pub const UNDEFINED: u32 = 5;
    pub const OBJECT: u32 = 6;
    pub const INT32: u32 = 7;
    pub const INT64: u32 = 10;
    pub const BOOL: u32 = 13;
    pub const REF: u32 = 15;
    /// Returned for a variable that has never been assigned.
    pub const UNSET: u32 = 0x00FF_FFFF;
    pub const MASK: u32 = 0x00FF_FFFF;
    /// Kinds that own heap data (refcounted). Never overwrite these by writing
    /// raw fields — the game would leak or double-free.
    pub const OWNING_MASK: u32 = (1 << STRING) | (1 << ARRAY) | (1 << OBJECT);
}

/// Ref (kind 15) payload tag for an object index: `OBJECT_REF_TAG | index`.
pub const OBJECT_REF_TAG: u64 = 0x0100_0000_0000_0000;

impl RValue {
    #[must_use]
    pub const fn real(v: f64) -> Self {
        Self {
            payload: v.to_bits(),
            flags: 0,
            kind: kind::REAL,
        }
    }

    /// GML `undefined`.
    #[must_use]
    pub const fn undefined() -> Self {
        Self {
            payload: 0,
            flags: 0,
            kind: kind::UNDEFINED,
        }
    }

    /// GML `true`/`false` (kind 13). Like the runner, the payload holds the
    /// value as a double (`1.0`/`0.0`).
    #[must_use]
    pub const fn boolean(v: bool) -> Self {
        Self {
            payload: if v { 0x3FF0_0000_0000_0000 } else { 0 },
            flags: 0,
            kind: kind::BOOL,
        }
    }

    /// A 64-bit integer (kind 10).
    #[must_use]
    pub const fn int64(v: i64) -> Self {
        Self {
            payload: v as u64,
            flags: 0,
            kind: kind::INT64,
        }
    }

    /// A ref (kind 15) with a raw payload, e.g. `OBJECT_REF_TAG | index`.
    #[must_use]
    pub const fn reference(payload: u64) -> Self {
        Self {
            payload,
            flags: 0,
            kind: kind::REF,
        }
    }

    #[must_use]
    pub const fn type_tag(&self) -> u32 {
        self.kind & kind::MASK
    }

    /// The value as a number, if it is numeric (real, int32, int64, bool).
    ///
    /// `None` for an int64 above 2^53 in magnitude: a double cannot hold it
    /// exactly, and such values are handles or references (ids from extension
    /// functions, script and method references near 2^56), where a rounded number names a
    /// different thing. Read those exactly with [`RValue::as_i64`].
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self.type_tag() {
            kind::REAL => Some(f64::from_bits(self.payload)),
            kind::INT32 => Some(f64::from(self.payload as u32 as i32)),
            kind::INT64 => int_as_f64(self.payload as i64),
            // The runner stores bools as double bits (1.0 = 0x3FF0…), some code
            // paths as integers; any non-zero payload is true.
            kind::BOOL => Some(if self.payload == 0 { 0.0 } else { 1.0 }),
            _ => None,
        }
    }

    /// The exact integer of an int32 or int64 (`None` for any other kind),
    /// for handles and references that [`RValue::as_f64`] cannot hold.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match self.type_tag() {
            kind::INT32 => Some(self.payload as u32 as i32 as i64),
            kind::INT64 => Some(self.payload as i64),
            _ => None,
        }
    }

    /// Whether this value owns heap data that the runtime must release.
    #[must_use]
    pub const fn is_owning(&self) -> bool {
        let tag = self.type_tag();
        tag < 32 && (kind::OWNING_MASK >> tag) & 1 != 0
    }
}

/// The largest integer magnitude a double holds exactly together with all
/// smaller ones (2^53).
const F64_EXACT_INT: u64 = 1 << 53;

/// An integer as a double, `None` when its magnitude is above 2^53 (see
/// [`RValue::as_f64`]); shared with [`crate::Value::as_f64`].
#[expect(
    clippy::cast_precision_loss,
    reason = "exact: the magnitude is checked against 2^53 first"
)]
pub(crate) fn int_as_f64(v: i64) -> Option<f64> {
    (v.unsigned_abs() <= F64_EXACT_INT).then_some(v as f64)
}

/// Variable slots at or above this are instance variables stored in the
/// instance's variable map and reachable via [`HostApi::instance_var`]. Lower
/// slots are built-ins (`x` = 0, `y` = 1, `image_angle` = 36, …) that the
/// runner stores in dedicated fields; use [`HostApi::builtin_get`] for those.
pub const USER_VAR_BASE: i32 = 100_000;

/// `array_index` value meaning "not an array access" for built-in variables.
pub const ARRAY_INDEX_NONE: i32 = i32::MIN;

/// Opaque GameMaker instance (`CInstance`). Only ever handled by pointer.
#[repr(C)]
#[derive(Debug)]
pub struct CInstance {
    _opaque: [u8; 0],
}

/// One in-flight call to a hooked GML function, shared along the hook chain.
///
/// For object events (`gml_Object_*`) `result`, `argc`, `argv` are null/0.
/// For scripts (`gml_Script_*`) they are the script's arguments and `ret` is
/// what the hooked function will return (initially `result`).
#[repr(C)]
#[derive(Debug)]
pub struct HookCall {
    pub self_inst: *mut CInstance,
    pub other_inst: *mut CInstance,
    pub result: *mut RValue,
    pub argc: i32,
    pub argv: *mut *mut RValue,
    pub ret: *mut RValue,
    /// Host-private: hook site index. Do not modify.
    pub site: u32,
    /// Host-private: next chain position. Do not modify.
    pub next: u32,
}

/// A mod's hook callback. Must call [`HostApi::call_next`] at most once to run
/// the rest of the chain (other mods, then the original function); not calling
/// it skips the original. The SDK wrapper handles this. `user` is the opaque
/// callback data given to [`HostApi::hook_gml`].
pub type HookCallback = unsafe extern "C-unwind" fn(call: *mut HookCall, user: *mut c_void);

/// Status codes returned by host functions.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    NotFound = 1,
    /// Hooks can only be registered from `ndml_mod_main`.
    HooksFrozen = 2,
    /// Name isn't a hookable kind (`gml_Object_*` events, `gml_Script_*`
    /// scripts and `builtin:<name>` built-in functions).
    Unsupported = 3,
    /// Out of hook thunks; raise `MAX_SITES` in the loader.
    Exhausted = 4,
    Internal = 5,
    /// The built-in variable has no setter.
    ReadOnly = 6,
    /// A runner table isn't available (yet); see the function's docs.
    NotReady = 7,
    /// An argument can't be passed to the game (e.g. a string containing NUL),
    /// or the value has the wrong type for the operation.
    ///
    /// Appended within ABI 1 (older loaders never return it).
    InvalidArgument = 8,
    /// A GML runtime error was raised inside the call and contained by the
    /// guard (`HostApi::guard_request` with `GUARD_CALLS`); the text is in the
    /// guard's reports. Appended within ABI 1.
    GmlError = 9,
}

impl Status {
    #[must_use]
    pub fn from_raw(raw: i32) -> Self {
        match raw {
            0 => Self::Ok,
            1 => Self::NotFound,
            2 => Self::HooksFrozen,
            3 => Self::Unsupported,
            4 => Self::Exhausted,
            6 => Self::ReadOnly,
            7 => Self::NotReady,
            8 => Self::InvalidArgument,
            9 => Self::GmlError,
            _ => Self::Internal,
        }
    }
}

/// Log levels for [`HostApi::log`] (match `log::Level` numbering).
#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

/// Function table the loader hands to each mod. Valid for the process lifetime.
#[repr(C)]
#[derive(Debug)]
pub struct HostApi {
    pub abi_version: u32,
    /// `size_of::<HostApi>()` as built by the host; fields past this are absent.
    pub struct_size: u32,
    pub log: unsafe extern "C" fn(mod_id: u32, level: LogLevel, msg: StrRef),
    /// Pointer to the runtime-assigned slot of a GML instance variable, or null
    /// if no variable with that name exists in the game. The slot is `-1`
    /// until the runner has initialised; read it at use time, not at init.
    pub resolve_variable: unsafe extern "C" fn(name: StrRef) -> *const i32,
    /// Register `cb` around the GML function `name` (e.g. `gml_Script_foo`).
    /// `user` is opaque callback data passed back to `cb` on every call.
    pub hook_gml:
        unsafe extern "C" fn(mod_id: u32, name: StrRef, cb: HookCallback, user: *mut c_void) -> i32,
    /// Continue the chain: next mod's callback, or the original function.
    pub call_next: unsafe extern "C-unwind" fn(call: *mut HookCall),
    /// Reference to an instance variable by slot, using the same accessors
    /// compiled GML uses: `write = false` → read reference, `true` → L-value
    /// reference (creates the variable if missing). Null if `inst` is null or
    /// `slot < USER_VAR_BASE` (built-ins are not supported by this path).
    pub instance_var:
        unsafe extern "C" fn(inst: *mut CInstance, slot: i32, write: bool) -> *mut RValue,
    /// Read built-in variable `name` (`x`, `y`, `speed`, `room_width`, ...) of
    /// `inst` into `*out` through the runner's own getter. `array_index` is
    /// [`ARRAY_INDEX_NONE`] for scalars. Returns a [`Status`] as `i32`:
    /// `NotFound` (no such built-in) or `NotReady` (runner table not found; only
    /// call from inside a hook, after the runner has started).
    ///
    /// Appended within ABI 1: check `struct_size` before use.
    pub builtin_get: unsafe extern "C-unwind" fn(
        inst: *mut CInstance,
        name: StrRef,
        array_index: i32,
        out: *mut RValue,
    ) -> i32,
    /// Write built-in variable `name` of `inst` from `*value` through the
    /// runner's own setter (which copies the value). `ReadOnly` for built-ins
    /// without a setter. Same caveats as [`HostApi::builtin_get`].
    pub builtin_set: unsafe extern "C-unwind" fn(
        inst: *mut CInstance,
        name: StrRef,
        array_index: i32,
        value: *const RValue,
    ) -> i32,
    /// The GML global instance (`global.*` variables; use `instance_var` on it),
    /// or null if not found / the runner hasn't started.
    ///
    /// Appended within ABI 1: check `struct_size` before use.
    pub global_instance: unsafe extern "C" fn() -> *mut CInstance,
    /// Whether the key with Windows virtual-key code `vk` is held down. Always
    /// `false` while the game window is not in the foreground.
    ///
    /// Appended within ABI 1: check `struct_size` before use.
    pub key_down: unsafe extern "C" fn(vk: u32) -> bool,

    // ---- Calls into game code (appended within ABI 1; check `struct_size`) ----
    /// Handle (the function's address) of the compiled GML script `name`
    /// (`gml_Script_*`, including `gml_Script_<fn>@gml_Object_…`), or null if
    /// there is none. Valid for the process lifetime; resolve during init.
    pub script_find: unsafe extern "C" fn(name: StrRef) -> *const c_void,
    /// Call a script found with `script_find` as compiled GML does:
    /// `f(self, other, result, argc, argv)`. `result` must be an `RValue` that
    /// owns nothing (e.g. `undefined`); afterwards it holds the return value,
    /// owned by the caller (release it with `value_free`). The callee may modify
    /// the arguments in place; they stay owned by the caller. A hooked script
    /// runs its hook chain. Only call from the game thread inside a hook.
    pub script_call: unsafe extern "C-unwind" fn(
        func: *const c_void,
        self_inst: *mut CInstance,
        other_inst: *mut CInstance,
        result: *mut RValue,
        argc: i32,
        argv: *mut *mut RValue,
    ) -> i32,
    /// Look up the GML builtin function `name` (`instance_exists`, `draw_text`,
    /// ...) and store its id in `*id`. `NotReady` until the runner has started
    /// (the table is filled at start-up; call from a hook), `NotFound` if the
    /// game has no such builtin.
    pub builtin_fn_find: unsafe extern "C-unwind" fn(name: StrRef, id: *mut i32) -> i32,
    /// Call builtin function `id` as the runner does:
    /// `routine(result, self, other, argc, args)` with `args` a contiguous array
    /// of `argc` values (borrowed). Same `result` contract as `script_call`.
    pub builtin_fn_call: unsafe extern "C-unwind" fn(
        id: i32,
        self_inst: *mut CInstance,
        other_inst: *mut CInstance,
        result: *mut RValue,
        argc: i32,
        args: *mut RValue,
    ) -> i32,
    /// Release `*out` (as `value_free`) and make it a new runner-owned string
    /// holding a copy of `s` (UTF-8). `InvalidArgument` if `s` contains NUL,
    /// `NotReady` if the runner's string routines weren't found.
    pub value_set_string: unsafe extern "C-unwind" fn(out: *mut RValue, s: StrRef) -> i32,
    /// Release what `*value` owns (string/array/struct reference) through the
    /// runner's own routine and set it to `undefined`. Non-owning values are
    /// just reset. `NotReady` (value untouched) if an owning value can't be
    /// released because the routine wasn't found.
    pub value_free: unsafe extern "C-unwind" fn(value: *mut RValue) -> i32,
    /// Borrow the bytes of string value `*value` into `*out` (valid while the
    /// value is alive and unchanged; usually UTF-8 but not guaranteed).
    /// `InvalidArgument` if it isn't a string.
    pub value_get_string: unsafe extern "C" fn(value: *const RValue, out: *mut StrRef) -> i32,
    /// GML `with (target)`: for each instance `target` selects (an instance id
    /// or an object ref, as the runner's own `with` resolves it), call
    /// `cb(user, inst, other)` with `inst` as `self` and the caller's `self_inst`
    /// as `other`, then restore. Returns the number of instances visited, or a
    /// negative [`Status`]: `InvalidArgument` for a target that isn't a ref or
    /// number (the runner would raise "invalid with reference"), `NotReady` if
    /// the runner's `with` routines weren't found. `self_inst`/`other_inst` must
    /// be live (the hook's own instances). Appended within ABI 1: check
    /// `struct_size` before use.
    pub with_instances: unsafe extern "C-unwind" fn(
        self_inst: *mut CInstance,
        other_inst: *mut CInstance,
        target: *const RValue,
        cb: WithCallback,
        user: *mut c_void,
    ) -> i32,
    /// GML error guard. Ask for (`on`) or drop the guards in `flags`:
    /// [`GUARD_EVENTS`] contains GML errors in the events of instances marked
    /// with `guard_mark` (and of events run inside guarded calls); it must be
    /// asked from `ndml_mod_main`. [`GUARD_CALLS`] turns a GML error inside a script or
    /// builtin call a mod makes into [`Status::GmlError`] instead of ending the
    /// process. Returns `Ok`; after init, `NotReady` if the runner pieces the
    /// guard needs weren't found (the guard stays off). Appended within ABI 1.
    pub guard_request: unsafe extern "C" fn(flags: u32, on: bool) -> i32,
    /// Replace the set of instance ids (`id` as the game reports it) whose
    /// events the event guard contains. Game thread. Appended within ABI 1.
    pub guard_mark: unsafe extern "C" fn(ids: *const i32, n: usize),
    /// Hand every GML error caught since the last call to `cb(user, report)`
    /// (the report and its text live for the callback only). Returns how
    /// many. Appended within ABI 1.
    pub guard_take: unsafe extern "C" fn(cb: GuardReportCallback, user: *mut c_void) -> u32,
    /// Event observer: call `cb(user, seen)` before and after every compiled
    /// event of kind `kind`, number `number` (`-1` = any) that any instance
    /// runs, e.g. user event 0 = kind 7, number 10. One shared detour, no hook
    /// site per object. Only from `ndml_mod_main` (`HooksFrozen` after);
    /// `Exhausted` past the loader's limit. If the runner pieces aren't found
    /// at boot the observer never runs (logged). Appended within ABI 1.
    pub observe_event: unsafe extern "C" fn(
        mod_id: u32,
        kind: i32,
        number: i32,
        cb: EventObserverCallback,
        user: *mut c_void,
    ) -> i32,
    /// Widen the event guard (needs [`GUARD_EVENTS`] asked at init) beyond
    /// the marked instances, for the event kinds in `kinds` (bit `1 << kind`:
    /// 0 create, 1 destroy, 2 alarm, 3 step, 4 collision, 7 other, 8 draw,
    /// 12 clean up, 14 pre-create): with [`GUARD_SCOPE_OFFSPRING`] in `flags`,
    /// every instance whose Create event starts while a contained event or a
    /// guarded call runs (what a contained instance creates, transitively;
    /// report `marked == 3`); and every instance whose `object_index` is one of the
    /// `n` in `objects` (report `marked == 2`). Replaces the previous scope;
    /// no flag and `n == 0` turn it off. Game thread. Appended within ABI 1.
    pub guard_scope: unsafe extern "C" fn(flags: u32, objects: *const i32, n: usize, kinds: u32),
    /// Open (`on`) or close an "errors expected" scope (the guard's calls
    /// must be on): GML errors caught while one is open are reported with
    /// [`GuardReport::expected`] set and logged as expected, not as contained
    /// errors. Scopes nest; a top-level event starting with one open closes
    /// them all. Returns the scopes now open. Game thread. Appended within
    /// ABI 1 (a host that has this field also fills `GuardReport::expected`).
    pub guard_expect: unsafe extern "C" fn(on: bool) -> u32,
}

/// What an event observer sees ([`HostApi::observe_event`]).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EventSeen {
    /// The instance running the event (`self`).
    pub self_inst: *mut CInstance,
    /// `other` for the event.
    pub other_inst: *mut CInstance,
    /// `self`'s instance id (`id` in GML).
    pub id: i32,
    /// `self`'s `object_index`.
    pub instance_object: i32,
    /// The object whose event code runs, as the dispatcher reports it (for an
    /// inherited event, the parent that defines it).
    pub object: i32,
    /// Event kind (`0` create, `3` step, `7` other/user, `8` draw, ...).
    pub kind: i32,
    /// Event number (user event `n` is number `10 + n` of kind 7).
    pub number: i32,
    /// 0 before the event runs, 1 after it returned.
    pub after: u32,
}

/// Callback for [`HostApi::observe_event`]. Runs on the game thread inside the
/// event dispatch, for every matching event: keep it short.
pub type EventObserverCallback =
    unsafe extern "C-unwind" fn(user: *mut c_void, seen: *const EventSeen);

/// [`HostApi::guard_request`] flag: contain errors in marked instances' events.
pub const GUARD_EVENTS: u32 = 1;
/// [`HostApi::guard_request`] flag: errors in mod calls become `GmlError`.
pub const GUARD_CALLS: u32 = 2;
/// [`HostApi::guard_scope`] flag: contain the offspring of contained events.
pub const GUARD_SCOPE_OFFSPRING: u32 = 1;

/// One GML error the guard caught ([`HostApi::guard_take`]).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GuardReport {
    /// 0: an event was abandoned; 1: a mod's call returned `GmlError`.
    pub caught: u32,
    /// The event's instance id (`-1` for a call).
    pub instance: i32,
    /// Its object index (`-1` for a call).
    pub object: i32,
    /// Why the event was guarded: 1 its instance was marked, 2 its object
    /// is in the object scope, 3 it is an offspring (`guard_scope`), 0 it ran
    /// inside a mod call.
    pub marked: u32,
    /// The runner's message (`ERROR in action number 1 of <event> for object
    /// <name>: <error>`), UTF-8.
    pub text: StrRef,
    /// 1: caught inside an "errors expected" scope (`HostApi::guard_expect`).
    /// Present only when the host has `guard_expect`. Appended within ABI 1.
    pub expected: u32,
}

/// Callback for [`HostApi::guard_take`].
pub type GuardReportCallback = unsafe extern "C" fn(user: *mut c_void, report: *const GuardReport);

/// Callback for [`HostApi::with_instances`]: `(user, self, other)`.
pub type WithCallback =
    unsafe extern "C-unwind" fn(user: *mut c_void, inst: *mut CInstance, other: *mut CInstance);

/// Passed to a mod's entry point.
#[repr(C)]
#[derive(Debug)]
pub struct ModInit {
    pub host: *const HostApi,
    pub mod_id: u32,
    /// Mod name (DLL file stem, e.g. `ndmod_magnet_range`).
    pub name: StrRef,
    /// Directory for this mod's config files (exists).
    pub config_dir: StrRef,
}

/// Signature of [`MOD_ENTRY_SYMBOL`]. Returns 0 on success.
pub type ModEntryFn = unsafe extern "C" fn(init: *const ModInit) -> i32;
/// Signature of [`MOD_ABI_SYMBOL`].
pub type ModAbiFn = unsafe extern "C" fn() -> u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_the_game() {
        assert_eq!(size_of::<RValue>(), 16);
        assert_eq!(core::mem::offset_of!(RValue, kind), 12);
    }

    #[test]
    fn rvalue_numeric_views() {
        assert_eq!(RValue::real(300.0).as_f64(), Some(300.0));
        let i = RValue {
            payload: u64::from((-5_i32) as u32),
            flags: 0,
            kind: kind::INT32,
        };
        assert_eq!(i.as_f64(), Some(-5.0));
        let s = RValue {
            payload: 0,
            flags: 0,
            kind: kind::STRING,
        };
        assert_eq!(s.as_f64(), None);
        assert!(s.is_owning());
        assert!(!RValue::real(1.0).is_owning());
    }

    /// An int64 is a number up to 2^53 in magnitude; past it `as_f64` gives
    /// `None` and `as_i64` the exact value. A method reference (tagged, about
    /// 2^56) would round to a neighbouring script.
    #[test]
    fn int64_beyond_2_pow_53_is_not_a_number() {
        let edge = 1_i64 << 53;
        assert_eq!(RValue::int64(edge).as_f64(), Some(9_007_199_254_740_992.0));
        assert_eq!(
            RValue::int64(-edge).as_f64(),
            Some(-9_007_199_254_740_992.0)
        );
        assert_eq!(
            RValue::int64(edge - 1).as_f64(),
            Some(9_007_199_254_740_991.0)
        );
        assert_eq!(RValue::int64(edge + 1).as_f64(), None);
        assert_eq!(RValue::int64(-edge - 1).as_f64(), None);
        assert_eq!(RValue::int64(i64::MIN).as_f64(), None);
        let method = 72_057_615_512_867_079_i64;
        assert_eq!(RValue::int64(method).as_f64(), None);
        assert_eq!(RValue::int64(method).as_i64(), Some(method));
        let handle = 9_007_199_254_753_337_i64; // 2^53 + 12345
        assert_eq!(RValue::int64(handle).as_f64(), None);
        assert_eq!(RValue::int64(handle).as_i64(), Some(handle));
        let i32_neg = RValue {
            payload: u64::from((-5_i32) as u32),
            flags: 0,
            kind: kind::INT32,
        };
        assert_eq!(i32_neg.as_i64(), Some(-5));
        assert_eq!(RValue::real(3.0).as_i64(), None);
    }

    #[test]
    fn rvalue_constructors() {
        assert_eq!(RValue::boolean(true).as_f64(), Some(1.0));
        assert_eq!(RValue::boolean(true).payload, 1.0f64.to_bits());
        assert_eq!(RValue::boolean(false).as_f64(), Some(0.0));
        // A bool the runner returns may hold `true` as double bits.
        let game_true = RValue {
            payload: 1.0f64.to_bits(),
            flags: 0,
            kind: kind::BOOL,
        };
        assert_eq!(game_true.as_f64(), Some(1.0));
        assert_eq!(RValue::int64(-7).as_f64(), Some(-7.0));
        assert_eq!(RValue::undefined().type_tag(), kind::UNDEFINED);
        let r = RValue::reference(OBJECT_REF_TAG | 0x25b);
        assert_eq!(
            (r.type_tag(), r.payload),
            (kind::REF, 0x0100_0000_0000_025b)
        );
        assert!(!r.is_owning());
    }

    #[test]
    fn status_round_trips() {
        for s in [Status::Ok, Status::NotReady, Status::InvalidArgument] {
            assert_eq!(Status::from_raw(s as i32), s);
        }
        assert_eq!(Status::from_raw(99), Status::Internal);
    }

    #[test]
    fn appended_fields_follow_older_ones() {
        // Append-only: every field added later must sit after `key_down`.
        let base = core::mem::offset_of!(HostApi, key_down);
        assert!(core::mem::offset_of!(HostApi, script_find) > base);
        assert!(
            core::mem::offset_of!(HostApi, value_get_string)
                > core::mem::offset_of!(HostApi, value_free)
        );
    }
}
