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

//! The SDK's only unsafe module: every raw pointer the loader hands a mod is
//! touched here and wrapped in a safe type.
//!
//! Mods are separate DLLs, so the loader can only reach them through a C ABI of raw pointers (`HostApi`, `HookCall`, `RValue*`,
//! `CInstance*`). Each block below states the contract that makes it sound.

#![allow(unsafe_code)]

use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::abi::{self, HookCall, HostApi, LogLevel, ModInit, RValue, Status, StrRef};

impl StrRef {
    /// # Safety
    /// `ptr`/`len` must describe valid UTF-8 that outlives `'a`.
    #[must_use]
    pub unsafe fn as_str<'a>(self) -> &'a str {
        if self.ptr.is_null() {
            return "";
        }
        // SAFETY: caller guarantees ptr/len describe live UTF-8.
        unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(self.ptr, self.len)) }
    }
}

/// Safe handle to the loader. Only constructed from a valid `ModInit`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Host {
    api: &'static HostApi,
    mod_id: u32,
}

impl Host {
    /// Whether the loader's `HostApi` is new enough to have `builtin_get/set`
    /// (appended within ABI 1; reading past `struct_size` would be out of bounds).
    fn has_builtins(self) -> bool {
        self.has_field(core::mem::offset_of!(HostApi, builtin_set))
    }

    /// Whether the loader's `HostApi` includes the pointer-sized field at `offset`
    /// (fields appended within ABI 1; reading past `struct_size` is out of bounds).
    fn has_field(self, offset: usize) -> bool {
        self.api.struct_size as usize >= offset + size_of::<usize>()
    }

    pub(crate) fn log(self, level: LogLevel, msg: &str) {
        // SAFETY: valid host fn; `msg` outlives the call.
        unsafe { (self.api.log)(self.mod_id, level, StrRef::new(msg)) };
    }

    pub(crate) fn resolve_variable(self, name: &str) -> Option<Variable> {
        // SAFETY: valid host fn; `name` is borrowed only for the call.
        let slot = unsafe { (self.api.resolve_variable)(StrRef::new(name)) };
        (!slot.is_null()).then_some(Variable { slot })
    }

    /// Register a `before`/`after` pair around a GML function.
    pub(crate) fn hook<S, B, A>(self, function: &str, before: B, after: A) -> Status
    where
        S: 'static,
        B: Fn(&mut Call<'_>) -> S + Send + Sync + 'static,
        A: Fn(&mut Call<'_>, S) + Send + Sync + 'static,
    {
        let state = Box::new(Wrapper {
            host: self,
            before,
            after,
            _s: PhantomData::<fn() -> S>,
        });
        let user = Box::into_raw(state).cast::<c_void>();
        // SAFETY: valid host fn; `user` is a leaked Box matched to the
        // monomorphised trampoline, kept alive for the process on success.
        let raw = unsafe {
            (self.api.hook_gml)(
                self.mod_id,
                StrRef::new(function),
                hook_trampoline::<S, B, A>,
                user,
            )
        };
        let status = Status::from_raw(raw);
        if status != Status::Ok {
            // SAFETY: the host rejected the hook and never stored `user`.
            drop(unsafe { Box::from_raw(user.cast::<Wrapper<S, B, A>>()) });
        }
        status
    }
}

/// A GML instance variable, resolved by name. Copy freely into hooks.
#[derive(Clone, Copy, Debug)]
pub struct Variable {
    slot: *const i32,
}

// SAFETY: `slot` points into the game's static data and is only read.
unsafe impl Send for Variable {}
// SAFETY: as above; concurrent reads of an i32 are fine.
unsafe impl Sync for Variable {}

impl Variable {
    /// The runtime slot index, or `None` before the runner has initialised.
    #[must_use]
    pub fn slot(self) -> Option<i32> {
        // SAFETY: non-null pointer from the host into static game data.
        let v = unsafe { self.slot.read_volatile() };
        (v >= 0).then_some(v)
    }
}

/// An in-progress call to a hooked GML function.
#[derive(Debug)]
pub struct Call<'a> {
    host: Host,
    raw: *mut HookCall,
    skip: bool,
    _marker: PhantomData<&'a mut HookCall>,
}

impl<'a> Call<'a> {
    fn raw(&self) -> &HookCall {
        // SAFETY: `raw` is the live HookCall for this callback; the host only
        // mutates it inside `call_next`, while no reference from here is held.
        unsafe { &*self.raw }
    }

    /// The instance running this code (`self` in GML), if any.
    #[must_use]
    pub fn instance(&self) -> Option<Instance<'a>> {
        Instance::new(self.host, self.raw().self_inst)
    }

    /// The `other` instance in GML terms, if any.
    #[must_use]
    pub fn other(&self) -> Option<Instance<'a>> {
        Instance::new(self.host, self.raw().other_inst)
    }

    /// Number of script arguments (0 for object events).
    #[must_use]
    pub fn argc(&self) -> usize {
        usize::try_from(self.raw().argc).unwrap_or(0)
    }

    /// Copy of script argument `i`.
    #[must_use]
    pub fn arg(&self, i: usize) -> Option<RValue> {
        let raw = self.raw();
        if i >= self.argc() || raw.argv.is_null() {
            return None;
        }
        // SAFETY: argv has argc valid entries for the duration of the call.
        let p = unsafe { *raw.argv.add(i) };
        // SAFETY: non-null argument pointers are valid RValues during the call.
        (!p.is_null()).then(|| unsafe { *p })
    }

    /// The script's return value (meaningful in `after` hooks).
    #[must_use]
    pub fn return_value(&self) -> Option<RValue> {
        let p = self.raw().ret;
        // SAFETY: `ret` is null or a valid RValue for the call's duration.
        (!p.is_null()).then(|| unsafe { *p })
    }

    /// The GML global instance, holding `global.*` variables (read them with
    /// [`Instance::get_f64`]). `None` if the host's `HostApi` lacks the field or
    /// the instance was not found.
    #[must_use]
    pub fn global(&self) -> Option<Instance<'a>> {
        if !self
            .host
            .has_field(core::mem::offset_of!(HostApi, global_instance))
        {
            return None;
        }
        // SAFETY: field present (checked); the host fn takes no arguments.
        Instance::new(self.host, unsafe { (self.host.api.global_instance)() })
    }

    /// In a `before` hook: don't run the original function (or later hooks).
    pub fn skip_original(&mut self) {
        self.skip = true;
    }
}

/// A live game instance. Only valid inside the hook callback it came from.
#[derive(Clone, Copy, Debug)]
pub struct Instance<'a> {
    host: Host,
    ptr: NonNull<abi::CInstance>,
    _marker: PhantomData<&'a ()>,
}

impl Instance<'_> {
    fn new(host: Host, ptr: *mut abi::CInstance) -> Option<Self> {
        NonNull::new(ptr).map(|ptr| Self {
            host,
            ptr,
            _marker: PhantomData,
        })
    }

    fn var_ptr(&self, var: Variable, write: bool) -> Option<NonNull<RValue>> {
        let slot = var.slot()?;
        // SAFETY: valid host fn; the instance is live during the callback.
        NonNull::new(unsafe { (self.host.api.instance_var)(self.ptr.as_ptr(), slot, write) })
    }

    /// Copy of an instance variable's value from the variable map (`None` for
    /// built-ins such as `x`/`y`, see [`abi::USER_VAR_BASE`]).
    #[must_use]
    pub fn get(&self, var: Variable) -> Option<RValue> {
        // SAFETY: pointer returned by the runtime's own accessor, valid now.
        self.var_ptr(var, false).map(|p| unsafe { p.read() })
    }

    /// An instance variable as a number, if it is numeric.
    #[must_use]
    pub fn get_f64(&self, var: Variable) -> Option<f64> {
        self.get(var)?.as_f64()
    }

    /// Set an instance variable to a real number. Refuses (returns `false`) if
    /// the current value owns heap data (string/array/struct), since replacing it
    /// without the runtime's free routine would leak or corrupt memory.
    #[must_use = "returns false if the value was not written"]
    pub fn set_f64(&self, var: Variable, value: f64) -> bool {
        let Some(p) = self.var_ptr(var, true) else {
            return false;
        };
        // SAFETY: valid runtime RValue pointer; the game loop is single-threaded.
        unsafe {
            if p.read().is_owning() {
                return false;
            }
            p.write(RValue::real(value));
        }
        true
    }

    /// Copy of built-in variable `name` (`x`, `y`, `speed`, `direction`,
    /// `room_width`, ...). `Err` if the loader doesn't provide built-ins, the
    /// name is unknown, or the runner table wasn't found.
    pub fn builtin(&self, name: &str) -> Result<RValue, Status> {
        if !self.host.has_builtins() {
            return Err(Status::Unsupported);
        }
        let mut out = RValue {
            payload: 0,
            flags: 0,
            kind: abi::kind::UNDEFINED,
        };
        // SAFETY: field present (checked via struct_size); the instance is live
        // during the callback; `out` is a valid RValue for the getter to fill.
        let raw = unsafe {
            (self.host.api.builtin_get)(
                self.ptr.as_ptr(),
                StrRef::new(name),
                abi::ARRAY_INDEX_NONE,
                &raw mut out,
            )
        };
        match Status::from_raw(raw) {
            Status::Ok => Ok(out),
            e => Err(e),
        }
    }

    /// A built-in variable as a number (`None` if unavailable or not numeric).
    #[must_use]
    pub fn builtin_f64(&self, name: &str) -> Option<f64> {
        self.builtin(name).ok()?.as_f64()
    }

    /// Set a built-in variable to a real number through the runner's setter.
    pub fn set_builtin_f64(&self, name: &str, value: f64) -> Result<(), Status> {
        if !self.host.has_builtins() {
            return Err(Status::Unsupported);
        }
        let value = RValue::real(value);
        // SAFETY: field present (checked); live instance; the setter copies `value`.
        let raw = unsafe {
            (self.host.api.builtin_set)(
                self.ptr.as_ptr(),
                StrRef::new(name),
                abi::ARRAY_INDEX_NONE,
                &raw const value,
            )
        };
        match Status::from_raw(raw) {
            Status::Ok => Ok(()),
            e => Err(e),
        }
    }
}

struct Wrapper<S, B, A> {
    host: Host,
    before: B,
    after: A,
    _s: PhantomData<fn() -> S>,
}

/// Monomorphised per hook; the host calls it with the `user` pointer registered
/// in `Host::hook`.
///
/// The original function is invoked *outside* `catch_unwind` so that foreign
/// (C++) exceptions thrown by the game propagate instead of aborting.
unsafe extern "C-unwind" fn hook_trampoline<S, B, A>(raw: *mut HookCall, user: *mut c_void)
where
    B: Fn(&mut Call<'_>) -> S,
    A: Fn(&mut Call<'_>, S),
{
    // SAFETY: `user` is the Box<Wrapper<S, B, A>> leaked in `Host::hook`.
    let w = unsafe { &*user.cast::<Wrapper<S, B, A>>() };
    let mut call = Call {
        host: w.host,
        raw,
        skip: false,
        _marker: PhantomData,
    };

    let state = catch_unwind(AssertUnwindSafe(|| (w.before)(&mut call)));
    if state.is_err() {
        log::error!("`before` hook panicked; running original anyway");
        call.skip = false;
    }
    if !call.skip {
        // SAFETY: `raw` is the live HookCall the host passed to this callback.
        unsafe { (w.host.api.call_next)(raw) };
    }
    if let Ok(state) = state {
        if catch_unwind(AssertUnwindSafe(|| (w.after)(&mut call, state))).is_err() {
            log::error!("`after` hook panicked");
        }
    }
}

/// Implementation detail of [`crate::export_mod!`]: decode the loader's
/// `ModInit`, install the logger, and run the mod's init inside `catch_unwind`.
///
/// # Safety
/// `init` must be the pointer the loader passes to `ndml_mod_main`.
#[doc(hidden)]
pub unsafe fn __run_mod_main(
    init: *const ModInit,
    f: fn(&crate::ModContext) -> Result<(), crate::Error>,
    defaults: &'static str,
) -> i32 {
    // SAFETY: loader contract: `init` and `init.host` are valid for the process
    // lifetime, and its StrRefs point at loader-owned UTF-8.
    let decoded = unsafe {
        init.as_ref().and_then(|init| {
            let api: &'static HostApi = init.host.as_ref()?;
            Some((
                api,
                init.mod_id,
                init.name.as_str(),
                init.config_dir.as_str(),
            ))
        })
    };
    let Some((api, mod_id, name, config_dir)) = decoded else {
        return -1;
    };
    if api.abi_version != abi::ABI_VERSION {
        return -1;
    }
    let host = Host { api, mod_id };
    crate::logger::install(host);
    let ctx = crate::ModContext::new(host, name.to_owned(), config_dir.into(), defaults);
    match catch_unwind(AssertUnwindSafe(|| f(&ctx))) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            log::error!("init failed: {e}");
            1
        }
        Err(_) => {
            log::error!("init panicked");
            2
        }
    }
}
