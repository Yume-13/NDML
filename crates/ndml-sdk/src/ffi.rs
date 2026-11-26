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

//! The SDK's only unsafe module: every raw pointer the loader hands a mod is
//! touched here and wrapped in a safe type.
//!
//! Mods are separate DLLs, so the loader can only reach them through a C ABI
//! of raw pointers (`HostApi`, `HookCall`, `RValue*`, `CInstance*`). Each block below
//! states the contract that makes it sound.

#![allow(unsafe_code)]

use core::ffi::c_void;
use core::marker::PhantomData;
use core::ptr::NonNull;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::abi::{self, HookCall, HostApi, LogLevel, ModInit, RValue, Status, StrRef};
use crate::value::Value;

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

    /// Whether the loader can call scripts/builtins and handle string values
    /// (the block of fields ending with `value_get_string`).
    pub(crate) fn has_calls(self) -> bool {
        self.has_field(core::mem::offset_of!(HostApi, value_get_string))
    }

    /// Resolve a `gml_Script_*` function for [`Call::call_script`].
    pub(crate) fn script(self, name: &str) -> Result<Script, crate::Error> {
        if !self.has_calls() {
            return Err(crate::Error::Other(
                "the installed loader is too old to call scripts".into(),
            ));
        }
        let full = if name.starts_with("gml_Script_") {
            name.to_owned()
        } else {
            format!("gml_Script_{name}")
        };
        // SAFETY: field present (checked); `full` is borrowed only for the call.
        let func = unsafe { (self.api.script_find)(StrRef::new(&full)) };
        if func.is_null() {
            return Err(crate::Error::FunctionNotFound(full));
        }
        Ok(Script {
            func,
            name: Box::leak(full.into_boxed_str()),
        })
    }

    /// A lazily resolved builtin function handle for [`Call::call_builtin`].
    pub(crate) fn builtin(self, name: &str) -> Result<Builtin, crate::Error> {
        if !self.has_calls() {
            return Err(crate::Error::Other(
                "the installed loader is too old to call builtin functions".into(),
            ));
        }
        // Handles are made once at init; leaking keeps them `Copy` and 'static.
        Ok(Builtin {
            name: Box::leak(name.to_owned().into_boxed_str()),
            id: Box::leak(Box::new(AtomicI32::new(BUILTIN_UNRESOLVED))),
        })
    }

    /// Release what `*v` owns (no-op for plain values) and reset it.
    fn free(self, v: &mut RValue) -> Status {
        if !v.is_owning() {
            *v = RValue::undefined();
            return Status::Ok;
        }
        // SAFETY: only called when `has_calls()` holds; `v` is a valid RValue the SDK owns.
        Status::from_raw(unsafe { (self.api.value_free)(v) })
    }

    /// Owned `RValue` for `value` (strings are created by the runner).
    fn encode(self, value: &Value) -> Result<RValue, Status> {
        if let Some(rv) = value.to_plain_rvalue() {
            return Ok(rv);
        }
        let Value::Str(s) = value else {
            return Err(Status::Internal);
        };
        let mut rv = RValue::undefined();
        // SAFETY: only called when `has_calls()` holds; `rv` owns nothing; `s`
        // is borrowed for the call (the runner copies it).
        let st =
            Status::from_raw(unsafe { (self.api.value_set_string)(&raw mut rv, StrRef::new(s)) });
        match st {
            Status::Ok => Ok(rv),
            e => Err(e),
        }
    }

    /// Owned copy of `*v` as a [`Value`] (strings are copied; `v` is untouched).
    fn decode(self, v: &RValue) -> Value {
        if let Some(plain) = Value::from_plain_rvalue(v) {
            return plain;
        }
        let mut s = StrRef {
            ptr: core::ptr::null(),
            len: 0,
        };
        // SAFETY: only called when `has_calls()` holds; `v` is a live string RValue.
        let st = Status::from_raw(unsafe { (self.api.value_get_string)(v, &raw mut s) });
        if st != Status::Ok || s.ptr.is_null() {
            return Value::Str(String::new());
        }
        // SAFETY: the host returned the string's bytes, valid while `v` is alive
        // (it is, for this call); copied before returning.
        let bytes = unsafe { core::slice::from_raw_parts(s.ptr, s.len) };
        Value::Str(String::from_utf8_lossy(bytes).into_owned())
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

/// A compiled GML script (`gml_Script_*`), resolved by name during init with
/// [`crate::ModContext::script`]. Copy freely into hooks; call it with
/// [`Call::call_script`] or [`Instance::call_script`].
#[derive(Clone, Copy, Debug)]
pub struct Script {
    func: *const c_void,
    name: &'static str,
}

// SAFETY: `func` is the address of game code, never dereferenced by the SDK;
// the host validates it on every call.
unsafe impl Send for Script {}
// SAFETY: as above.
unsafe impl Sync for Script {}

impl Script {
    /// The full function name (`gml_Script_…`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

const BUILTIN_UNRESOLVED: i32 = -1;
const BUILTIN_MISSING: i32 = -2;

/// A GML builtin function (`instance_exists`, `draw_text`, ...), named during
/// init with [`crate::ModContext::builtin`]. The runner fills its builtin table
/// only at start-up, so the name is checked on the first call (inside a hook)
/// and the id cached; an unknown name then fails with `Status::NotFound`.
#[derive(Clone, Copy, Debug)]
pub struct Builtin {
    name: &'static str,
    id: &'static AtomicI32,
}

impl Builtin {
    /// The builtin's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    fn resolve(self, host: Host) -> Result<i32, Status> {
        match self.id.load(Ordering::Relaxed) {
            BUILTIN_MISSING => return Err(Status::NotFound),
            id if id >= 0 => return Ok(id),
            _ => {}
        }
        let mut id = BUILTIN_UNRESOLVED;
        // SAFETY: `Builtin`s are only made when `has_calls()` holds; `name` is
        // borrowed for the call and `id` is a valid out-pointer.
        let st = Status::from_raw(unsafe {
            (host.api.builtin_fn_find)(StrRef::new(self.name), &raw mut id)
        });
        match st {
            Status::Ok => {
                self.id.store(id, Ordering::Relaxed);
                Ok(id)
            }
            Status::NotFound => {
                self.id.store(BUILTIN_MISSING, Ordering::Relaxed);
                log::warn!("unable to find builtin `{}`", self.name);
                Err(Status::NotFound)
            }
            e => Err(e), // NotReady: retry next time
        }
    }
}

/// Owned `RValue`s (arguments, results) released through the runner when
/// dropped. Never outlives the callback it was made in.
struct Temps {
    host: Host,
    vals: Vec<RValue>,
}

impl Temps {
    fn new(host: Host, n: usize) -> Self {
        Self {
            host,
            vals: Vec::with_capacity(n),
        }
    }

    fn push(&mut self, v: &Value) -> Result<(), Status> {
        let rv = self.host.encode(v)?;
        self.vals.push(rv);
        Ok(())
    }

    /// Move value `i` out (it won't be released on drop).
    fn take(&mut self, i: usize) -> RValue {
        core::mem::replace(&mut self.vals[i], RValue::undefined())
    }
}

impl Drop for Temps {
    fn drop(&mut self) {
        for v in &mut self.vals {
            let _ = self.host.free(v);
        }
    }
}

/// Self/other pointers for a call into game code.
#[derive(Clone, Copy)]
struct Target {
    host: Host,
    self_inst: *mut abi::CInstance,
    other_inst: *mut abi::CInstance,
}

impl Target {
    fn check(self) -> Result<Self, Status> {
        if !self.host.has_calls() {
            return Err(Status::Unsupported);
        }
        if self.self_inst.is_null() {
            return Err(Status::InvalidArgument);
        }
        Ok(self)
    }

    fn script(self, script: Script, args: &[Value]) -> Result<Value, Status> {
        let t = self.check()?;
        let mut temps = Temps::new(t.host, args.len() + 1);
        temps.push(&Value::Undefined)?; // [0] = result
        for a in args {
            temps.push(a)?;
        }
        let n_args = i32::try_from(args.len()).map_err(|_| Status::InvalidArgument)?;
        let base = temps.vals.as_mut_ptr();
        // SAFETY: `base` points at `args.len() + 1` initialised RValues owned by
        // `temps`, which is not touched until the call returns.
        let mut arg_ptrs: Vec<*mut RValue> =
            (1..=args.len()).map(|i| unsafe { base.add(i) }).collect();
        // SAFETY: field present (`check`); live instances from the hook; `base`
        // is the result slot (owns nothing) and `arg_ptrs` holds `n_args` pointers to
        // owned arguments, released by `temps` afterwards.
        let st = Status::from_raw(unsafe {
            (t.host.api.script_call)(
                script.func,
                t.self_inst,
                t.other_inst,
                base,
                n_args,
                arg_ptrs.as_mut_ptr(),
            )
        });
        if st != Status::Ok {
            return Err(st);
        }
        Ok(t.host.decode(&temps.vals[0]))
    }

    fn builtin(self, builtin: Builtin, args: &[Value]) -> Result<Value, Status> {
        let t = self.check()?;
        let id = builtin.resolve(t.host)?;
        let mut result = Temps::new(t.host, 1);
        result.push(&Value::Undefined)?;
        let mut temps = Temps::new(t.host, args.len());
        for a in args {
            temps.push(a)?;
        }
        let n_args = i32::try_from(args.len()).map_err(|_| Status::InvalidArgument)?;
        // SAFETY: field present (`check`); live instances from the hook; the
        // result slot owns nothing; `temps.vals` is a contiguous array of `n_args`
        // owned arguments the routine borrows; both are released afterwards.
        let st = Status::from_raw(unsafe {
            (t.host.api.builtin_fn_call)(
                id,
                t.self_inst,
                t.other_inst,
                result.vals.as_mut_ptr(),
                n_args,
                temps.vals.as_mut_ptr(),
            )
        });
        if st != Status::Ok {
            return Err(st);
        }
        Ok(t.host.decode(&result.vals[0]))
    }
}

impl Target {
    /// [`Target::builtin`] with arguments that may be held game values, and the
    /// result kept (not decoded, not released) for the caller.
    fn builtin_held(self, builtin: Builtin, args: &[Arg<'_, '_>]) -> Result<RValue, Status> {
        let t = self.check()?;
        let id = builtin.resolve(t.host)?;
        let mut result = Temps::new(t.host, 1);
        result.push(&Value::Undefined)?;
        // Owned (encoded) arguments are released afterwards; held ones are
        // bitwise copies the routine only borrows, so they are not.
        let mut owned = Temps::new(t.host, args.len());
        let mut vals: Vec<RValue> = Vec::with_capacity(args.len());
        for a in args {
            match a {
                Arg::Value(v) => {
                    owned.push(v)?;
                    vals.push(owned.vals[owned.vals.len() - 1]);
                }
                Arg::Held(h) => vals.push(h.rv),
            }
        }
        let n_args = i32::try_from(args.len()).map_err(|_| Status::InvalidArgument)?;
        // SAFETY: field present (`check`); live instances from the hook; the
        // result slot owns nothing; `vals` is a contiguous array of `n_args`
        // values the routine borrows: copies of `owned`'s values (released by
        // `owned` afterwards, once) and of held values (released by their
        // `Held`), none of which the routine frees.
        let st = Status::from_raw(unsafe {
            (t.host.api.builtin_fn_call)(
                id,
                t.self_inst,
                t.other_inst,
                result.vals.as_mut_ptr(),
                n_args,
                vals.as_mut_ptr(),
            )
        });
        if st != Status::Ok {
            return Err(st);
        }
        Ok(result.take(0))
    }
}

/// A game value the mod holds on to: the result of
/// [`Call::call_builtin_held`], typically an array, struct or method the
/// [`Value`] conversion cannot represent. It can be passed back to builtins
/// ([`Arg::Held`]) to read or change it (`array_length`, `array_get`,
/// `variable_struct_get_names`, `array_set`, `variable_instance_set`, ...),
/// and is released through the runner when dropped. Only valid inside the
/// hook call it came from.
#[derive(Debug)]
pub struct Held<'a> {
    host: Host,
    rv: RValue,
    _marker: PhantomData<&'a ()>,
}

impl Held<'_> {
    /// The value as a [`Value`] (arrays, structs and methods are
    /// [`Value::Other`]; strings are copied).
    #[must_use]
    pub fn value(&self) -> Value {
        self.host.decode(&self.rv)
    }

    /// The runner's type tag (`abi::kind`).
    #[must_use]
    pub fn kind(&self) -> u32 {
        self.rv.type_tag()
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let _ = self.host.free(&mut self.rv);
    }
}

/// An argument for [`Call::call_builtin_held`].
#[derive(Debug, Clone, Copy)]
pub enum Arg<'v, 'a> {
    /// A plain value (encoded for the call, released afterwards).
    Value(&'v Value),
    /// A value held by the mod (borrowed by the call).
    Held(&'v Held<'a>),
}

/// State for one [`Target::with_each`] run, shared with [`with_trampoline`].
struct WithCtx<'f> {
    host: Host,
    f: &'f mut dyn FnMut(Instance<'_>, Option<Instance<'_>>),
    panicked: bool,
}

/// Called by the loader once per instance of a `with`.
unsafe extern "C-unwind" fn with_trampoline(
    user: *mut c_void,
    inst: *mut abi::CInstance,
    other: *mut abi::CInstance,
) {
    // SAFETY: `user` is the `WithCtx` on `with_each`'s stack, alive for the
    // whole `with_instances` call; nothing else touches it meanwhile.
    let ctx = unsafe { &mut *user.cast::<WithCtx<'_>>() };
    let Some(i) = Instance::new(ctx.host, inst) else {
        return;
    };
    let o = Instance::new(ctx.host, other);
    if catch_unwind(AssertUnwindSafe(|| (ctx.f)(i, o))).is_err() {
        ctx.panicked = true;
    }
}

impl Target {
    fn with_each(
        self,
        target: &Value,
        f: &mut dyn FnMut(Instance<'_>, Option<Instance<'_>>),
    ) -> Result<u32, Status> {
        let t = self.check()?;
        if !t
            .host
            .has_field(core::mem::offset_of!(HostApi, with_instances))
        {
            return Err(Status::Unsupported);
        }
        let rv = match target {
            Value::Ref(_) | Value::Real(_) | Value::Int(_) => {
                target.to_plain_rvalue().ok_or(Status::InvalidArgument)?
            }
            _ => return Err(Status::InvalidArgument),
        };
        let mut ctx = WithCtx {
            host: t.host,
            f,
            panicked: false,
        };
        // SAFETY: field present (checked); live instances from the hook; `rv`
        // is a plain value the loader copies; `ctx` outlives the call and is
        // only used by `with_trampoline` during it.
        let n = unsafe {
            (t.host.api.with_instances)(
                t.self_inst,
                t.other_inst,
                &raw const rv,
                with_trampoline,
                (&raw mut ctx).cast(),
            )
        };
        if ctx.panicked {
            log::error!("a `with` callback panicked (the iteration went on)");
        }
        u32::try_from(n).map_err(|_| Status::from_raw(-n))
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
    /// [`Instance::get_f64`]). `None` if the loader is too old or didn't find it.
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

    /// Whether the key with virtual-key code `vk` is held down (see
    /// [`crate::input::vk_code`]). `false` while the game window isn't in the
    /// foreground, or if the loader is too old to report keys.
    #[must_use]
    pub fn key_down(&self, vk: u32) -> bool {
        if !self
            .host
            .has_field(core::mem::offset_of!(HostApi, key_down))
        {
            return false;
        }
        // SAFETY: field present (checked); the host fn takes a plain integer.
        unsafe { (self.host.api.key_down)(vk) }
    }

    /// In a `before` hook: don't run the original function (or later hooks).
    pub fn skip_original(&mut self) {
        self.skip = true;
    }

    /// Script argument `i` as an owned [`Value`] (strings copied).
    #[must_use]
    pub fn arg_value(&self, i: usize) -> Option<Value> {
        let a = self.arg(i)?;
        if a.type_tag() == abi::kind::STRING && !self.host.has_calls() {
            return None;
        }
        Some(self.host.decode(&a))
    }

    /// The script's return value as an owned [`Value`] (meaningful in `after`).
    #[must_use]
    pub fn result(&self) -> Option<Value> {
        let r = self.return_value()?;
        if r.type_tag() == abi::kind::STRING && !self.host.has_calls() {
            return None;
        }
        Some(self.host.decode(&r))
    }

    /// Replace the hooked script's return value. Use it in an `after` hook (or in
    /// a `before` hook that calls [`Call::skip_original`]; otherwise the original
    /// overwrites it). The old value is released through the runner.
    /// `Err` for object events (no return value) or if the loader is too old.
    pub fn set_return(&mut self, value: impl Into<Value>) -> Result<(), crate::Error> {
        let err = |status| crate::Error::Value {
            op: "set_return",
            status,
        };
        if !self.host.has_calls() {
            return Err(err(Status::Unsupported));
        }
        let ret = self.raw().ret;
        if ret.is_null() {
            return Err(err(Status::Unsupported));
        }
        let mut new = Temps::new(self.host, 1);
        new.push(&value.into()).map_err(err)?;
        // SAFETY: `ret` is the RValue the hooked script returns, valid for the
        // call; the game owns it, so release its old payload before replacing.
        let old = unsafe { &mut *ret };
        match self.host.free(old) {
            Status::Ok => {
                *old = new.take(0);
                Ok(())
            }
            e => Err(err(e)),
        }
    }

    /// The instances a call from this hook runs as: the hook's `self` (or the
    /// global instance if it has none) and `other` (or `self`).
    fn target(&self) -> Target {
        let raw = self.raw();
        let self_inst = if raw.self_inst.is_null() {
            self.global()
                .map_or(core::ptr::null_mut(), |g| g.ptr.as_ptr())
        } else {
            raw.self_inst
        };
        let other_inst = if raw.other_inst.is_null() {
            self_inst
        } else {
            raw.other_inst
        };
        Target {
            host: self.host,
            self_inst,
            other_inst,
        }
    }

    /// Call a GML script with this hook's `self`/`other` and return its result.
    /// Arguments are released afterwards, and so is the game's copy of the
    /// result (strings are copied into the [`Value`]; arrays/structs come back
    /// as [`Value::Other`]).
    ///
    /// Runs game code synchronously on the game thread. If the script is hooked
    /// (by any mod, including this one) its hook chain runs; don't hold a lock
    /// your own hooks take while calling. A GML runtime error inside the call
    /// ends the game process (see `docs/mod-api.md`), unless the
    /// loader's call guard is on ([`crate::ModContext::guard`]): then it
    /// returns `Err(Error::Call { status: Status::GmlError, .. })`.
    pub fn call_script(&self, script: Script, args: &[Value]) -> Result<Value, crate::Error> {
        self.target()
            .script(script, args)
            .map_err(|status| crate::Error::Call {
                function: script.name.to_owned(),
                status,
            })
    }

    /// Call a GML builtin function with this hook's `self`/`other`. Same value
    /// handling and caveats as [`Call::call_script`]. Drawing builtins only have
    /// a visible effect inside Draw events.
    pub fn call_builtin(&self, builtin: Builtin, args: &[Value]) -> Result<Value, crate::Error> {
        self.target()
            .builtin(builtin, args)
            .map_err(|status| crate::Error::Call {
                function: builtin.name.to_owned(),
                status,
            })
    }

    /// Call a GML builtin and keep its result as a [`Held`] game value
    /// (arrays, structs and methods stay usable: pass them back as
    /// [`Arg::Held`]). Same caveats as [`Call::call_builtin`].
    pub fn call_builtin_held<'c>(
        &'c self,
        builtin: Builtin,
        args: &[Arg<'_, 'c>],
    ) -> Result<Held<'c>, crate::Error> {
        self.target()
            .builtin_held(builtin, args)
            .map(|rv| Held {
                host: self.host,
                rv,
                _marker: PhantomData,
            })
            .map_err(|status| crate::Error::Call {
                function: builtin.name.to_owned(),
                status,
            })
    }

    /// GML `with (target)`: run `f` once for each instance `target` selects
    /// (an instance id as the game returns it, or `Value::object(index)` for
    /// every instance of an object and its children), with that instance as
    /// `self` and this hook's `self` as `other`. Inside `f` the instance can do
    /// everything a hook's own instance can (`call_script`, `set`, builtins), so
    /// this is how a mod reaches an instance it only knows by id. Returns how
    /// many instances ran. `Err(InvalidArgument)` for a target that isn't a ref
    /// or number; an id that no longer exists just visits none. Don't destroy
    /// other instances of the same set inside `f`.
    pub fn with(
        &self,
        target: &Value,
        mut f: impl FnMut(Instance<'_>, Option<Instance<'_>>),
    ) -> Result<u32, crate::Error> {
        self.target()
            .with_each(target, &mut f)
            .map_err(|status| crate::Error::Call {
                function: "with".to_owned(),
                status,
            })
    }

    /// Whether the runner's function table has `builtin` (checks the name
    /// without calling it; `Err(NotReady)` before the table is filled). Covers
    /// extension functions too, which the runner registers in the same table.
    pub fn builtin_available(&self, builtin: Builtin) -> Result<bool, Status> {
        if !self.host.has_calls() {
            return Err(Status::Unsupported);
        }
        match builtin.resolve(self.host) {
            Ok(_) => Ok(true),
            Err(Status::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
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

    /// Opaque identity of this instance (its address), stable while the
    /// instance exists. Useful as a map key for per-instance state; an address
    /// may be reused after the instance is destroyed.
    #[must_use]
    pub fn key(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    /// Copy of an instance variable's value from the variable map (`None` for
    /// built-ins such as `x`/`y`, see [`abi::USER_VAR_BASE`]).
    #[must_use]
    pub fn get(&self, var: Variable) -> Option<RValue> {
        // SAFETY: pointer returned by the runtime's own accessor, valid now.
        self.var_ptr(var, false).map(|p| unsafe { p.read() })
    }

    /// An instance variable as a number, if it is numeric (`None` for an
    /// int64 beyond 2^53, see [`RValue::as_f64`]).
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
    /// name is unknown, or the runner table wasn't found. Built-ins holding a
    /// string or array give `Err(InvalidArgument)` (the game's reference is
    /// released); read those with [`Instance::builtin_value`].
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
            Status::Ok if out.is_owning() => {
                // The getter returned a reference (string/array), which can't be
                // returned raw without leaking it. Release it if possible.
                if self.host.has_calls() {
                    let _ = self.host.free(&mut out);
                }
                Err(Status::InvalidArgument)
            }
            Status::Ok => Ok(out),
            e => Err(e),
        }
    }

    /// Built-in variable `name` as an owned [`Value`]: like [`Instance::builtin`]
    /// but strings are copied and the game's reference released, so string
    /// built-ins don't leak.
    pub fn builtin_value(&self, name: &str) -> Result<Value, crate::Error> {
        let err = |status| crate::Error::Value {
            op: "builtin_value",
            status,
        };
        if !self.host.has_builtins() || !self.host.has_calls() {
            return Err(err(Status::Unsupported));
        }
        let mut out = Temps::new(self.host, 1);
        out.push(&Value::Undefined).map_err(err)?;
        // SAFETY: fields present (checked); the instance is live during the
        // callback; the slot is a valid, non-owning RValue for the getter, and
        // `out` releases whatever it receives.
        let raw = unsafe {
            (self.host.api.builtin_get)(
                self.ptr.as_ptr(),
                StrRef::new(name),
                abi::ARRAY_INDEX_NONE,
                out.vals.as_mut_ptr(),
            )
        };
        match Status::from_raw(raw) {
            Status::Ok => Ok(self.host.decode(&out.vals[0])),
            e => Err(err(e)),
        }
    }

    /// An instance variable as an owned [`Value`] (strings copied).
    #[must_use]
    pub fn get_value(&self, var: Variable) -> Option<Value> {
        let v = self.get(var)?;
        if v.type_tag() == abi::kind::STRING && !self.host.has_calls() {
            return None;
        }
        Some(self.host.decode(&v))
    }

    /// Set an instance variable to any [`Value`] (strings are created by
    /// the runner). Unlike [`Instance::set_f64`] this also replaces strings,
    /// arrays and structs, releasing the old value through the runner.
    pub fn set(&self, var: Variable, value: impl Into<Value>) -> Result<(), crate::Error> {
        let err = |status| crate::Error::Value { op: "set", status };
        if !self.host.has_calls() {
            return Err(err(Status::Unsupported));
        }
        let Some(p) = self.var_ptr(var, true) else {
            return Err(err(Status::NotFound));
        };
        let mut new = Temps::new(self.host, 1);
        new.push(&value.into()).map_err(err)?;
        // SAFETY: L-value reference from the runtime's own accessor, valid now;
        // the game loop is single-threaded. The variable owns its old value.
        let slot = unsafe { &mut *p.as_ptr() };
        match self.host.free(slot) {
            Status::Ok => {
                *slot = new.take(0);
                Ok(())
            }
            e => Err(err(e)),
        }
    }

    /// Call a GML script with this instance as `self` and `other`. See
    /// [`Call::call_script`] for value handling and caveats.
    pub fn call_script(&self, script: Script, args: &[Value]) -> Result<Value, crate::Error> {
        self.target()
            .script(script, args)
            .map_err(|status| crate::Error::Call {
                function: script.name.to_owned(),
                status,
            })
    }

    /// Call a GML builtin function with this instance as `self` and `other`
    /// (e.g. `instance_destroy()` with no arguments destroys it).
    pub fn call_builtin(&self, builtin: Builtin, args: &[Value]) -> Result<Value, crate::Error> {
        self.target()
            .builtin(builtin, args)
            .map_err(|status| crate::Error::Call {
                function: builtin.name.to_owned(),
                status,
            })
    }

    /// Run `f` for each instance `target` names (an id, an object index or
    /// `all`), with this instance as `other`, as [`Call::with`] does: how
    /// code that holds only an instance (an event observer) reaches another
    /// one by id. Same caveats as [`Call::with`].
    pub fn with(
        &self,
        target: &Value,
        mut f: impl FnMut(Instance<'_>, Option<Instance<'_>>),
    ) -> Result<u32, crate::Error> {
        self.target()
            .with_each(target, &mut f)
            .map_err(|status| crate::Error::Call {
                function: "with".to_owned(),
                status,
            })
    }

    fn target(&self) -> Target {
        Target {
            host: self.host,
            self_inst: self.ptr.as_ptr(),
            other_inst: self.ptr.as_ptr(),
        }
    }

    /// A built-in variable as a number (`None` if unavailable or not numeric,
    /// or an int64 beyond 2^53: see [`Value::as_f64`]).
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

// ---- GML error guard ----

/// One GML error the loader's guard caught (see [`Call::guard_take`]).
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // one flag per reason, as the report gives them
pub struct GmlError {
    /// `true`: a script/builtin call of a mod returned `Status::GmlError`;
    /// `false`: an event was abandoned.
    pub in_call: bool,
    /// The event's instance id (`-1` for a call).
    pub instance: i32,
    /// The instance's object index (`-1` for a call).
    pub object: i32,
    /// The instance was marked with [`Call::guard_mark`] (else the event ran
    /// inside a mod's guarded call, or its object is in the scope).
    pub marked: bool,
    /// Contained by the wider scope of [`Call::guard_scope`]: as an offspring
    /// of a contained event or call.
    pub offspring: bool,
    /// Contained by the wider scope: its object is named there.
    pub by_object: bool,
    /// The runner's message: `ERROR in action number 1 of <event> for object
    /// <name>: <error>`.
    pub text: String,
    /// Caught inside an "errors expected" scope ([`Call::guard_expect`];
    /// always `false` with a loader that has none).
    pub expected: bool,
}

/// An open "errors expected" scope ([`Call::guard_expect`]); closes on drop.
#[derive(Debug)]
#[must_use = "the scope closes when this is dropped"]
pub struct ExpectErrors<'c> {
    host: Host,
    open: bool,
    _call: PhantomData<&'c ()>,
}

impl Drop for ExpectErrors<'_> {
    fn drop(&mut self) {
        if self.open {
            self.host.guard_expect(false);
        }
    }
}

/// The id number of an instance as the game hands it out: an instance ref
/// (`0x0400_0001 << 32 | id`) or a plain number.
#[must_use]
pub fn instance_number(v: &Value) -> Option<i32> {
    match *v {
        Value::Ref(p) if p >> 32 == 0x0400_0001 => Some(p as u32 as i32),
        Value::Real(f) if f.fract() == 0.0 && f >= 0.0 && f <= f64::from(i32::MAX) => {
            Some(f as i32)
        }
        Value::Int(i) => i32::try_from(i).ok(),
        _ => None,
    }
}

impl Host {
    fn has_guard(self) -> bool {
        self.has_field(core::mem::offset_of!(HostApi, guard_take))
    }

    pub(crate) fn guard_request(self, flags: u32, on: bool) -> Status {
        if !self.has_guard() {
            return Status::Unsupported;
        }
        // SAFETY: field present (checked); plain arguments.
        Status::from_raw(unsafe { (self.api.guard_request)(flags, on) })
    }

    fn guard_mark(self, ids: &[i32]) {
        if self.has_guard() {
            // SAFETY: field present; `ids` lives for the call.
            unsafe { (self.api.guard_mark)(ids.as_ptr(), ids.len()) };
        }
    }

    fn guard_scope(self, offspring: bool, objects: &[i32], kinds: u32) -> bool {
        if !self.has_field(core::mem::offset_of!(HostApi, guard_scope)) {
            return false;
        }
        let flags = if offspring {
            abi::GUARD_SCOPE_OFFSPRING
        } else {
            0
        };
        // SAFETY: field present (checked); `objects` lives for the call.
        unsafe { (self.api.guard_scope)(flags, objects.as_ptr(), objects.len(), kinds) };
        true
    }

    pub(crate) fn has_expect(self) -> bool {
        self.has_field(core::mem::offset_of!(HostApi, guard_expect))
    }

    /// Open (`on`) or close an "errors expected" scope; `false` from a loader
    /// without them.
    pub(crate) fn guard_expect(self, on: bool) -> bool {
        if !self.has_expect() {
            return false;
        }
        // SAFETY: field present (checked); plain argument.
        unsafe { (self.api.guard_expect)(on) };
        true
    }

    fn guard_take(self) -> Vec<GmlError> {
        /// One report; `expected` only from a host that fills it (one with
        /// `guard_expect`: the report struct grew with it).
        fn push(out: &mut Vec<GmlError>, r: &abi::GuardReport, expected: bool) {
            let bytes = if r.text.ptr.is_null() {
                &[][..]
            } else {
                // SAFETY: the report's text is valid UTF-8 bytes for the
                // callback's duration (host contract; copied lossily).
                unsafe { core::slice::from_raw_parts(r.text.ptr, r.text.len) }
            };
            out.push(GmlError {
                in_call: r.caught == 1,
                instance: r.instance,
                object: r.object,
                marked: r.marked == 1,
                by_object: r.marked == 2,
                offspring: r.marked == 3,
                text: String::from_utf8_lossy(bytes).into_owned(),
                expected,
            });
        }
        unsafe extern "C" fn collect(user: *mut c_void, r: *const abi::GuardReport) {
            if user.is_null() || r.is_null() {
                return;
            }
            // SAFETY: `user` is the `Vec` below; `r` and its text are valid for
            // this call (host contract). An older host's report ends before
            // `expected`, which is not read here.
            let (out, r) = unsafe { (&mut *user.cast::<Vec<GmlError>>(), &*r) };
            push(out, r, false);
        }
        unsafe extern "C" fn collect_expected(user: *mut c_void, r: *const abi::GuardReport) {
            if user.is_null() || r.is_null() {
                return;
            }
            // SAFETY: as in `collect`; a host with `guard_expect` passes the
            // whole report, `expected` included.
            let (out, r) = unsafe { (&mut *user.cast::<Vec<GmlError>>(), &*r) };
            push(out, r, r.expected != 0);
        }
        let mut out: Vec<GmlError> = Vec::new();
        if self.has_guard() {
            let cb: abi::GuardReportCallback = if self.has_expect() {
                collect_expected
            } else {
                collect
            };
            // SAFETY: field present; `cb` matches the callback type and `out`
            // outlives the call.
            unsafe { (self.api.guard_take)(cb, (&raw mut out).cast()) };
        }
        out
    }
}

impl Call<'_> {
    /// Replace the set of instances whose events the loader's guard contains
    /// (ids as the game hands them out; others are ignored). Needs the event
    /// guard asked for at init ([`crate::ModContext::guard`]).
    pub fn guard_mark(&self, ids: &[Value]) {
        let ids: Vec<i32> = ids.iter().filter_map(instance_number).collect();
        self.host.guard_mark(&ids);
    }

    /// Widen the event guard beyond the marked instances, for the event kinds
    /// in `kinds` (bit `1 << kind`): `offspring`, every instance whose Create
    /// event starts while a contained event or guarded call runs (what a
    /// contained instance creates, transitively); and every instance of the
    /// given object indices. The event guard must have been asked for at init. Replaces
    /// the previous scope; `false` and empty turn it off. Returns `false` from
    /// a loader without it.
    #[must_use]
    pub fn guard_scope(&self, offspring: bool, objects: &[i32], kinds: u32) -> bool {
        self.host.guard_scope(offspring, objects, kinds)
    }

    /// Every GML error the guard caught since the last call (any mod's).
    #[must_use]
    pub fn guard_take(&self) -> Vec<GmlError> {
        self.host.guard_take()
    }

    /// Open an "errors expected" scope for as long as the returned value
    /// lives: GML errors the guard catches meanwhile (in a call this mod
    /// makes, or an event it runs) are reported with
    /// [`GmlError::expected`] and logged by the loader as expected, not as
    /// contained errors. Keep it around the one call that may fail on
    /// purpose. A loader without scopes ignores it.
    pub fn guard_expect(&self) -> ExpectErrors<'_> {
        ExpectErrors {
            host: self.host,
            open: self.host.guard_expect(true),
            _call: PhantomData,
        }
    }

    /// Turn the call guard on or off from a hook (`Err(NotReady)`: the
    /// runner pieces it needs weren't found; `Err(Unsupported)`: old loader).
    pub fn guard_calls(&self, on: bool) -> Result<(), Status> {
        match self.host.guard_request(abi::GUARD_CALLS, on) {
            Status::Ok => Ok(()),
            e => Err(e),
        }
    }
}

// ---- event observers ----

/// One event an observer saw ([`crate::ModContext::observe_event`]).
#[derive(Clone, Copy, Debug)]
pub struct ObservedEvent<'a> {
    host: Host,
    self_inst: *mut abi::CInstance,
    other_inst: *mut abi::CInstance,
    /// `self`'s instance id.
    pub id: i32,
    /// `self`'s `object_index`.
    pub instance_object: i32,
    /// The object whose event code runs (a parent's for an inherited event).
    pub object: i32,
    /// Event kind (`7` for user events).
    pub kind: i32,
    /// Event number (user event `n` is `10 + n`).
    pub number: i32,
    /// `false` before the event runs, `true` after it returned.
    pub after: bool,
    _marker: PhantomData<&'a ()>,
}

impl<'a> ObservedEvent<'a> {
    /// The instance running the event.
    #[must_use]
    pub fn instance(&self) -> Option<Instance<'a>> {
        Instance::new(self.host, self.self_inst)
    }

    /// `other` for the event.
    #[must_use]
    pub fn other(&self) -> Option<Instance<'a>> {
        Instance::new(self.host, self.other_inst)
    }
}

struct ObserverBox<F> {
    host: Host,
    f: F,
}

unsafe extern "C-unwind" fn observer_trampoline<F>(user: *mut c_void, seen: *const abi::EventSeen)
where
    F: Fn(&ObservedEvent<'_>),
{
    if user.is_null() || seen.is_null() {
        return;
    }
    // SAFETY: `user` is the Box<ObserverBox<F>> leaked in `Host::observe_event`;
    // `seen` is valid for this call (host contract).
    let (w, s) = unsafe { (&*user.cast::<ObserverBox<F>>(), &*seen) };
    let event = ObservedEvent {
        host: w.host,
        self_inst: s.self_inst,
        other_inst: s.other_inst,
        id: s.id,
        instance_object: s.instance_object,
        object: s.object,
        kind: s.kind,
        number: s.number,
        after: s.after != 0,
        _marker: PhantomData,
    };
    if catch_unwind(AssertUnwindSafe(|| (w.f)(&event))).is_err() {
        log::error!("event observer panicked");
    }
}

impl Host {
    pub(crate) fn observe_event<F>(self, kind: i32, number: i32, f: F) -> Status
    where
        F: Fn(&ObservedEvent<'_>) + Send + Sync + 'static,
    {
        if !self.has_field(core::mem::offset_of!(HostApi, observe_event)) {
            return Status::Unsupported;
        }
        let user = Box::into_raw(Box::new(ObserverBox { host: self, f })).cast::<c_void>();
        // SAFETY: field present (checked); `user` is a leaked Box matched to
        // the monomorphised trampoline, kept alive for the process on success.
        let raw = unsafe {
            (self.api.observe_event)(self.mod_id, kind, number, observer_trampoline::<F>, user)
        };
        let status = Status::from_raw(raw);
        if status != Status::Ok {
            // SAFETY: the host refused and never stored `user`.
            drop(unsafe { Box::from_raw(user.cast::<ObserverBox<F>>()) });
        }
        status
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
    if let Ok(state) = state
        && catch_unwind(AssertUnwindSafe(|| (w.after)(&mut call, state))).is_err()
    {
        log::error!("`after` hook panicked");
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
    // SAFETY: the caller's contract (`# Safety`) makes `init` the loader's
    // pointer. The loader (`mods::load_one`) builds the `ModInit` on its stack:
    // `init` and its StrRefs are valid only for this call, and both strings are
    // copied below (`to_owned`, `into`) before it returns. `init.host` is
    // `&HOST_API`, a `static` in the loader DLL; the game imports that DLL as
    // `version.dll`, so it stays mapped until the process exits, which is what
    // the `&'static HostApi` relies on. The loader's StrRefs point at UTF-8 it
    // made from `&str`.
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
            log::error!("unable to start ({e})");
            1
        }
        Err(_) => {
            log::error!("init panicked");
            2
        }
    }
}
