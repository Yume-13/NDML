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

impl RValue {
    #[must_use]
    pub const fn real(v: f64) -> Self {
        Self {
            payload: v.to_bits(),
            flags: 0,
            kind: kind::REAL,
        }
    }

    #[must_use]
    pub const fn type_tag(&self) -> u32 {
        self.kind & kind::MASK
    }

    /// The value as a number, if it is numeric (real, int32, int64, bool).
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self.type_tag() {
            kind::REAL => Some(f64::from_bits(self.payload)),
            kind::INT32 => Some(f64::from(self.payload as u32 as i32)),
            #[allow(clippy::cast_precision_loss)]
            kind::INT64 => Some(self.payload as i64 as f64),
            kind::BOOL => Some(if self.payload as u32 == 0 { 0.0 } else { 1.0 }),
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

/// Variable slots at or above this are instance variables stored in the
/// instance's variable map and reachable via [`HostApi::instance_var`]. Lower
/// slots are built-ins (`x`, `y`, `image_angle`, …) that the runner stores in
/// dedicated fields; use [`HostApi::builtin_get`] for those.
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
    /// Name isn't a hookable kind (`gml_Object_*` events and `gml_Script_*`).
    Unsupported = 3,
    /// Out of hook thunks; raise `MAX_SITES` in the loader.
    Exhausted = 4,
    Internal = 5,
    /// The built-in variable has no setter.
    ReadOnly = 6,
    /// A runner table isn't available (yet); see the function's docs.
    NotReady = 7,
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
    /// Reference to an instance variable in the variable map, by slot, using the
    /// same accessors compiled GML uses: `write = false` → read reference, `true` → L-value
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
}

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
}
