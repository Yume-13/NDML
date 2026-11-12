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

//! The [`HostApi`] function table handed to mods.

#![allow(unsafe_code)] // C functions called by mods through raw pointers

use core::ffi::c_void;
use core::ops::Range;
use std::collections::HashMap;
use std::sync::OnceLock;

use ndml_sdk::abi::{
    ABI_VERSION, CInstance, HookCallback, HostApi, LogLevel, RValue, Status, StrRef, USER_VAR_BASE,
};

use crate::yyc::RunnerIndex;

static INDEX: OnceLock<RunnerIndex> = OnceLock::new();
static MOD_NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();

pub static HOST_API: HostApi = HostApi {
    abi_version: ABI_VERSION,
    struct_size: size_of::<HostApi>() as u32,
    log: host_log,
    resolve_variable,
    hook_gml,
    call_next: crate::hooks::call_next,
    instance_var,
    builtin_get,
    builtin_set,
    global_instance,
};

/// Where the game image's sections are, for runtime table discovery.
#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub base: usize,
    pub size: usize,
    /// Absolute `.text` range.
    pub text: Range<usize>,
    /// Absolute `.data` range (including its zero-filled tail, i.e. `.bss`).
    pub data: Range<usize>,
}

static IMAGE: OnceLock<ImageInfo> = OnceLock::new();

pub fn set_image(info: ImageInfo) {
    let _ = IMAGE.set(info);
}

pub fn set_index(index: RunnerIndex) {
    let _ = INDEX.set(index);
}

pub fn index() -> Option<&'static RunnerIndex> {
    INDEX.get()
}

/// Record mod names (indexed by mod id) for log prefixes.
pub fn set_mod_names(names: Vec<&'static str>) {
    let _ = MOD_NAMES.set(names);
}

fn mod_name(mod_id: u32) -> &'static str {
    MOD_NAMES
        .get()
        .and_then(|n| n.get(mod_id as usize).copied())
        .unwrap_or("mod")
}

unsafe extern "C" fn host_log(mod_id: u32, level: LogLevel, msg: StrRef) {
    let level = match level {
        LogLevel::Error => log::Level::Error,
        LogLevel::Warn => log::Level::Warn,
        LogLevel::Info => log::Level::Info,
        LogLevel::Debug => log::Level::Debug,
        LogLevel::Trace => log::Level::Trace,
    };
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let msg = unsafe { msg.as_str() };
    log::log!(target: mod_name(mod_id), level, "{msg}");
}

unsafe extern "C" fn resolve_variable(name: StrRef) -> *const i32 {
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    index()
        .and_then(|i| i.variables.get(name))
        .map_or(core::ptr::null(), |&a| a as *const i32)
}

unsafe extern "C" fn hook_gml(
    mod_id: u32,
    name: StrRef,
    cb: HookCallback,
    user: *mut c_void,
) -> i32 {
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    let status = match index() {
        Some(index) => crate::hooks::register(index, mod_id, name, cb, user),
        None => Status::Internal,
    };
    if status == Status::Ok {
        log::debug!(target: mod_name(mod_id), "hooked {name}");
    } else {
        log::warn!(target: mod_name(mod_id), "hook {name} refused: {status:?}");
    }
    status as i32
}

/// `YYObjectBase` vtable slots used by compiled GML for instance variables:
/// `RValue& InternalGetYYVarRef(int slot)` (reads) and
/// `RValue& InternalGetYYVarRefL(int slot)` (writes; creates if missing).
const VTABLE_GET_VAR_REF: usize = 1;
const VTABLE_GET_VAR_REF_L: usize = 2;

unsafe extern "C" fn instance_var(inst: *mut CInstance, slot: i32, write: bool) -> *mut RValue {
    type GetVarRef = unsafe extern "C" fn(*mut CInstance, i32) -> *mut RValue;
    if inst.is_null() || slot < USER_VAR_BASE {
        return core::ptr::null_mut();
    }
    let index = if write {
        VTABLE_GET_VAR_REF_L
    } else {
        VTABLE_GET_VAR_REF
    };
    // SAFETY: `inst` is a live CInstance (C++ object; vtable pointer first). The
    // functions at these vtable slots have the GetVarRef signature (see
    // `docs/how-it-works.md`).
    unsafe {
        let vtable = *inst.cast::<*const usize>();
        let f: GetVarRef = std::mem::transmute(*vtable.add(index));
        f(inst, slot)
    }
}

type BuiltinGetter = unsafe extern "C-unwind" fn(*mut CInstance, i32, *mut RValue) -> u8;
type BuiltinSetter = unsafe extern "C-unwind" fn(*mut CInstance, i32, *const RValue) -> u8;

#[derive(Clone, Copy)]
struct BuiltinVar {
    getter: BuiltinGetter,
    setter: Option<BuiltinSetter>,
}

/// Built-in variables by name, discovered on first use (the runner fills its
/// table during start-up, after the loader's boot scan). `Err` is cached: by the
/// time a hook runs the runner is initialised, so a miss won't fix itself.
static BUILTINS: OnceLock<Result<HashMap<String, BuiltinVar>, String>> = OnceLock::new();

fn builtins() -> Result<&'static HashMap<String, BuiltinVar>, Status> {
    let table = BUILTINS.get_or_init(|| {
        let t = discover_builtins();
        match &t {
            Ok(m) => log::info!("built-in variable table: {} entries", m.len()),
            Err(e) => log::error!("built-in variable table not found: {e}"),
        }
        t
    });
    table.as_ref().map_err(|_| Status::NotReady)
}

fn discover_builtins() -> Result<HashMap<String, BuiltinVar>, String> {
    let img = IMAGE.get().ok_or("image info not set")?;
    let data = crate::win::copy_image_range(img.base, img.size, img.data.start, img.data.len())
        .ok_or(".data outside image")?;
    let entries = crate::yyc::find_builtin_table(&data, img.data.start, &img.text)
        .ok_or("no table-shaped run in .data")?;
    let mut map = HashMap::with_capacity(entries.len());
    for e in entries {
        let Some(name) = crate::win::read_c_str(e.name, 64) else {
            continue;
        };
        // SAFETY: `find_builtin_table` only accepts entries whose getter/setter
        // point into `.text`; the runner registers them with these signatures
        // (called from the runner's `DoPushBuiltin` / `SetValue`, see
        // `docs/how-it-works.md`).
        let var = unsafe {
            BuiltinVar {
                getter: std::mem::transmute::<usize, BuiltinGetter>(e.getter),
                setter: e
                    .setter
                    .map(|p| std::mem::transmute::<usize, BuiltinSetter>(p)),
            }
        };
        map.entry(name).or_insert(var);
    }
    // Sanity check by name: every GameMaker runner has these.
    for must in ["x", "y", "speed", "room_width"] {
        if !map.contains_key(must) {
            return Err(format!(
                "candidate table lacks `{must}` ({} names)",
                map.len()
            ));
        }
    }
    Ok(map)
}

unsafe extern "C-unwind" fn builtin_get(
    inst: *mut CInstance,
    name: StrRef,
    array_index: i32,
    out: *mut RValue,
) -> i32 {
    if inst.is_null() || out.is_null() {
        return Status::Internal as i32;
    }
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    let var = match builtins() {
        Ok(m) => match m.get(name) {
            Some(v) => *v,
            None => return Status::NotFound as i32,
        },
        Err(s) => return s as i32,
    };
    // SAFETY: getter from the runner's table; `inst` is a live instance and
    // `out` a valid RValue, as the SDK guarantees.
    unsafe { (var.getter)(inst, array_index, out) };
    Status::Ok as i32
}

unsafe extern "C-unwind" fn builtin_set(
    inst: *mut CInstance,
    name: StrRef,
    array_index: i32,
    value: *const RValue,
) -> i32 {
    if inst.is_null() || value.is_null() {
        return Status::Internal as i32;
    }
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    let var = match builtins() {
        Ok(m) => match m.get(name) {
            Some(v) => *v,
            None => return Status::NotFound as i32,
        },
        Err(s) => return s as i32,
    };
    let Some(setter) = var.setter else {
        return Status::ReadOnly as i32;
    };
    // SAFETY: setter from the runner's table; live instance; the setter copies
    // `*value`, which the SDK keeps alive for the call.
    unsafe { setter(inst, array_index, value) };
    Status::Ok as i32
}

unsafe extern "C" fn global_instance() -> *mut CInstance {
    let Some(addr) = index().and_then(|i| i.global_instance_ptr) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `addr` is a pointer-sized, aligned slot in the EXE's `.data`
    // (found by `yyc::scan`), mapped for the process lifetime. The runner sets
    // it at start-up; until then it reads as null.
    unsafe { (addr as *const *mut CInstance).read_volatile() }
}
