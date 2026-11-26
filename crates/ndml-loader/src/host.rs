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

//! The [`HostApi`] function table handed to mods.
//!
//! Besides variable access this is where mods call into game code: compiled
//! scripts (by address from the function table), builtin functions (through the
//! runner's builtin function table, found by shape on first use) and the
//! runner's own string-create and `RValue`-release routines (found by code
//! shape and verified with a self-test). Every lookup that fails logs once and
//! disables only that feature: the call returns `NotReady`.

#![allow(unsafe_code)] // C functions called by mods through raw pointers

use core::ffi::c_void;
use core::ops::Range;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use ndml_sdk::abi::{
    ABI_VERSION, CInstance, EventObserverCallback, GuardReport, GuardReportCallback, HookCallback,
    HostApi, LogLevel, RValue, Status, StrRef, USER_VAR_BASE, WithCallback, kind,
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
    key_down,
    script_find,
    script_call,
    builtin_fn_find,
    builtin_fn_call,
    value_set_string,
    value_free,
    value_get_string,
    with_instances,
    guard_request,
    guard_mark,
    guard_take,
    observe_event,
    guard_scope,
    guard_expect,
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
    /// The mapped image (`bytes[rva]`), for code-shape scans.
    pub bytes: &'static [u8],
    /// Its PE layout (sections, for `.pdata` lookups).
    pub layout: crate::pe::Layout,
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
    let result = match index() {
        Some(index) => crate::hooks::register(index, mod_id, name, cb, user),
        None => Err(Status::Internal.into()),
    };
    match result {
        Ok(()) => {
            log::debug!(target: mod_name(mod_id), "hooked {name}");
            Status::Ok as i32
        }
        Err(r) => {
            let why = r.why.map_or_else(String::new, |w| format!(" ({w})"));
            log::warn!(target: mod_name(mod_id), "hook {name} refused: {:?}{why}", r.status);
            r.status as i32
        }
    }
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
            Ok(m) => log::debug!("built-in variable table: {} entries", m.len()),
            Err(e) => log::error!("unable to find the built-in variable table ({e})"),
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
        // (called from the runner's `DoPushBuiltin` / `SetValue`).
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

unsafe extern "C" fn key_down(vk: u32) -> bool {
    crate::win::key_down(vk)
}

// ---- Calling scripts --------------------------------------------------------

/// `RValue* gml_Script_<name>(self, other, result, argc, argv)`.
type ScriptFn = unsafe extern "C-unwind" fn(
    *mut CInstance,
    *mut CInstance,
    *mut RValue,
    i32,
    *mut *mut RValue,
) -> *mut RValue;

/// Addresses of every `gml_Script_*` function, to validate handles.
static SCRIPTS: OnceLock<HashSet<usize>> = OnceLock::new();

fn script_addresses() -> &'static HashSet<usize> {
    SCRIPTS.get_or_init(|| {
        index().map_or_else(HashSet::new, |i| {
            i.functions
                .iter()
                .filter(|(n, _)| n.starts_with("gml_Script_"))
                .map(|(_, &a)| a)
                .collect()
        })
    })
}

unsafe extern "C" fn script_find(name: StrRef) -> *const c_void {
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    if !name.starts_with("gml_Script_") {
        return core::ptr::null();
    }
    index()
        .and_then(|i| i.functions.get(name))
        .map_or(core::ptr::null(), |&a| a as *const c_void)
}

#[allow(clippy::similar_names)] // argc/argv are the conventional names
unsafe extern "C-unwind" fn script_call(
    func: *const c_void,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    result: *mut RValue,
    argc: i32,
    argv: *mut *mut RValue,
) -> i32 {
    if result.is_null() || argc < 0 || (argc > 0 && argv.is_null()) {
        return Status::InvalidArgument as i32;
    }
    if !script_addresses().contains(&(func as usize)) {
        return Status::NotFound as i32;
    }
    // SAFETY: `func` is the entry of a compiled `gml_Script_*` function from
    // the runner's function table (checked above), which has the script
    // signature. If it is hooked, its detour runs the
    // hook chain, which holds no locks. Scripts reset `*result` without
    // releasing it, so the SDK passes one that owns nothing.
    let ret = if crate::guard::calls_on() {
        let args = [
            self_inst as u64,
            other_inst as u64,
            result as u64,
            u64::from(argc as u32),
            argv as u64,
            0,
        ];
        // SAFETY: as above; the guard frame passes the same five arguments.
        match unsafe { crate::hooks::guarded_script_call(func as usize, args) } {
            Ok(r) => r as *mut RValue,
            Err((thrown, draw)) => {
                // The error is read before the draw state is put back.
                let status = call_failed(thrown, "script");
                draw.finish();
                return status;
            }
        }
    } else {
        // SAFETY: as above.
        unsafe {
            let f: ScriptFn = std::mem::transmute::<*const c_void, ScriptFn>(func);
            f(self_inst, other_inst, result, argc, argv)
        }
    };
    if !ret.is_null() && ret != result {
        // Compiled scripts always return `result`; another pointer would
        // need a reference-counted copy, which is not made here.
        log::warn!("script at {func:p} returned a foreign RValue; result ignored");
    }
    Status::Ok as i32
}

// ---- Hooking builtin functions ---------------------------------------------

/// Builtin name → routine, from the runner's static registrations (works at
/// boot, before the runtime table exists). `None`: not found (logged once).
static BUILTIN_ROUTINES: OnceLock<Option<HashMap<String, usize>>> = OnceLock::new();

/// The routine of builtin `name`, for a hook: `NotReady` if the registrations
/// weren't found (builtin hooks unavailable), `NotFound` for an unknown name.
pub fn builtin_routine(name: &str) -> Result<usize, Status> {
    let map = BUILTIN_ROUTINES.get_or_init(|| {
        let img = IMAGE.get()?;
        let t = std::time::Instant::now();
        let image = crate::yyc::Image {
            base: img.base,
            bytes: img.bytes,
        };
        let found = crate::yyc::find_builtin_routines(image, &img.layout);
        match &found {
            Some(m) => log::debug!(
                "builtin registrations: {} names (scan {:?}); builtin hooks available",
                m.len(),
                t.elapsed()
            ),
            None => log::warn!("unable to find the builtin registrations; builtin hooks off"),
        }
        found
    });
    match map {
        None => Err(Status::NotReady),
        Some(m) => m.get(name).copied().ok_or(Status::NotFound),
    }
}

// ---- Calling builtin functions ---------------------------------------------

/// `void routine(RValue* result, CInstance* self, CInstance* other, int argc, RValue* args)`.
type BuiltinRoutine =
    unsafe extern "C-unwind" fn(*mut RValue, *mut CInstance, *mut CInstance, i32, *mut RValue);

/// The runner's builtin function table, discovered on first use.
#[derive(Debug)]
struct FunctionTable {
    /// Indexed by the runner's function id.
    routines: Vec<Option<usize>>,
    by_name: HashMap<String, i32>,
}

static FUNCTIONS: OnceLock<Result<FunctionTable, String>> = OnceLock::new();

/// Builtins every GameMaker runner registers; a table without them is wrong.
const REQUIRED_BUILTINS: [&str; 4] = [
    "instance_exists",
    "draw_text",
    "variable_instance_get",
    "string_upper",
];

/// The builtin function table, discovered on the first call after the game has
/// started running GML (it is filled during runner start-up). Before that this
/// returns `NotReady` without caching, so a too-early call doesn't poison it.
fn functions() -> Result<&'static FunctionTable, Status> {
    if FUNCTIONS.get().is_none() && !crate::hooks::dispatched() {
        return Err(Status::NotReady);
    }
    let table = FUNCTIONS.get_or_init(|| {
        let t = discover_functions();
        match &t {
            Ok(f) => log::debug!("builtin function table: {} functions", f.by_name.len()),
            Err(e) => log::error!("unable to find the builtin function table ({e})"),
        }
        t
    });
    table.as_ref().map_err(|_| Status::NotReady)
}

fn discover_functions() -> Result<FunctionTable, String> {
    let img = IMAGE.get().ok_or("image info not set")?;
    let data = crate::win::copy_image_range(img.base, img.size, img.data.start, img.data.len())
        .ok_or(".data outside image")?;
    let image_range = img.base..img.base + img.size;
    let candidates = crate::yyc::function_table_candidates(&data, img.data.start, &image_range);
    let n_candidates = candidates.len();
    for header in candidates {
        let off = header - img.data.start;
        let ptr = u64::from_le_bytes(data[off..off + 8].try_into().map_err(|_| "short")?) as usize;
        let count = u32::from_le_bytes(data[off + 8..off + 12].try_into().map_err(|_| "short")?);
        let Some(bytes) =
            crate::win::read_memory(ptr, count as usize * crate::yyc::FUNCTION_ENTRY_SIZE)
        else {
            continue;
        };
        let Some(entries) = crate::yyc::parse_function_entries(&bytes, &img.text) else {
            continue;
        };
        let mut by_name = HashMap::with_capacity(entries.len());
        let mut routines = Vec::with_capacity(entries.len());
        for (id, e) in entries.iter().enumerate() {
            let named = e.and_then(|e| Some((crate::win::read_c_str(e.name, 128)?, e.routine)));
            routines.push(named.as_ref().map(|&(_, r)| r));
            if let (Some((name, _)), Ok(id)) = (named, i32::try_from(id)) {
                by_name.entry(name).or_insert(id);
            }
        }
        if REQUIRED_BUILTINS.iter().all(|n| by_name.contains_key(*n)) {
            log::debug!("builtin function table header at {header:#x} ({count} entries)");
            return Ok(FunctionTable { routines, by_name });
        }
    }
    Err(format!(
        "{n_candidates} candidate header(s), none holds {REQUIRED_BUILTINS:?}"
    ))
}

unsafe extern "C-unwind" fn builtin_fn_find(name: StrRef, id: *mut i32) -> i32 {
    if id.is_null() {
        return Status::InvalidArgument as i32;
    }
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let name = unsafe { name.as_str() };
    match functions() {
        Ok(t) => match t.by_name.get(name).copied().or_else(|| extension_id(name)) {
            Some(found) => {
                // SAFETY: `id` is a valid out-pointer (checked non-null; SDK contract).
                unsafe { id.write(found) };
                Status::Ok as i32
            }
            None => Status::NotFound as i32,
        },
        Err(s) => s as i32,
    }
}

#[allow(clippy::similar_names)] // argc/args mirror the runner's routine signature
unsafe extern "C-unwind" fn builtin_fn_call(
    id: i32,
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    result: *mut RValue,
    argc: i32,
    args: *mut RValue,
) -> i32 {
    if result.is_null() || argc < 0 || (argc > 0 && args.is_null()) {
        return Status::InvalidArgument as i32;
    }
    let table = match functions() {
        Ok(t) => t,
        Err(s) => return s as i32,
    };
    let routine = if id >= EXTENSION_ID_BASE {
        extension_routine(id)
    } else {
        usize::try_from(id)
            .ok()
            .and_then(|i| table.routines.get(i).copied().flatten())
    };
    let Some(routine) = routine else {
        return Status::NotFound as i32;
    };
    // SAFETY: `routine` comes from the runner's builtin function table (entry
    // validated to point into `.text`), registered with the builtin routine
    // signature (as compiled code's `CALL_BUILTIN` calls it), or is an extension
    // DLL export whose mangled name encodes that same signature
    // (`extension_id`). `args` holds `argc`
    // values the SDK keeps alive; `result` owns nothing, as the runner's own
    // dispatcher guarantees before each call.
    if crate::guard::calls_on() {
        let a = [
            result as u64,
            self_inst as u64,
            other_inst as u64,
            u64::from(argc as u32),
            args as u64,
            0,
        ];
        // SAFETY: as above; the guard frame passes the same five arguments.
        if let Err(thrown) = unsafe { crate::hooks::guarded_call(routine, a) } {
            return call_failed(thrown, "builtin");
        }
    } else {
        // SAFETY: as above.
        unsafe {
            let f = std::mem::transmute::<usize, BuiltinRoutine>(routine);
            f(result, self_inst, other_inst, argc, args);
        }
    }
    Status::Ok as i32
}

// ---- GML error guard ----

/// A guarded mod call raised a GML error: log it, queue the report.
fn call_failed(thrown: [u64; 2], what: &str) -> i32 {
    let text = error_text(thrown);
    let expected = crate::guard::expecting();
    if expected {
        crate::guard::log_expected(
            -1,
            &format!("a mod's {what} call"),
            &text.trim().replace('\n', " | "),
        );
    } else {
        // Rate-limited as the event guard's lines are (`guard::log_due`): a
        // call failing every frame is not 60 log writes a second.
        let line = text.trim().replace('\n', " | ");
        if let Some(n) = crate::guard::log_due(-1, &line) {
            log::warn!(
                "GML error in a mod's {what} call (returned GmlError, #{n} of this error): {line}"
            );
        }
    }
    crate::guard::push_report(crate::guard::Report {
        caught: crate::guard::Caught::Call,
        instance: -1,
        object: -1,
        why: crate::guard::Why::InCall,
        text,
        expected,
    });
    Status::GmlError as i32
}

/// The message of a caught `YYGMLException`: the runner throws a struct with
/// `message`, `longMessage`, `script`, `line` and `stacktrace`; read
/// `longMessage` (it names the object and event) with `variable_struct_get`,
/// else `message`. Game thread, right after the catch (the struct is
/// collectable garbage from then on).
#[must_use]
pub fn error_text(thrown: [u64; 2]) -> String {
    let ex = RValue {
        payload: thrown[0],
        flags: thrown[1] as u32,
        kind: (thrown[1] >> 32) as u32,
    };
    if ex.type_tag() != kind::OBJECT {
        return format!("(thrown value of kind {})", ex.type_tag());
    }
    let text = struct_string(ex, "longMessage")
        .or_else(|| struct_string(ex, "message"))
        .unwrap_or_else(|| "(error text unreadable)".to_owned());
    // Where: the compiled function and its line (the message does not always
    // name the variable; the line does).
    match (struct_string(ex, "script"), struct_string(ex, "line")) {
        (Some(script), Some(line)) => format!("{}\nat {script} (line {line})", text.trim_end()),
        (Some(script), None) => format!("{}\nat {script}", text.trim_end()),
        _ => text,
    }
}

/// `variable_struct_get(ex, field)` as a string (a number is formatted),
/// through the guard (an error here is contained too and gives `None`).
fn struct_string(ex: RValue, field: &str) -> Option<String> {
    let table = functions().ok()?;
    let id = *table.by_name.get("variable_struct_get")?;
    let routine = table
        .routines
        .get(usize::try_from(id).ok()?)
        .copied()
        .flatten()?;
    let create = create_string_fn().ok()?;
    let name = std::ffi::CString::new(field).ok()?;
    let me = global_instance_ptr()?;
    let mut args = [ex, RValue::undefined()];
    let mut result = RValue::undefined();
    // SAFETY: verified create-string routine; `args[1]` owns nothing; `name`
    // is NUL-terminated and outlives the call (the runner copies it).
    unsafe { create(&raw mut args[1], name.as_ptr().cast()) };
    let a = [
        (&raw mut result) as u64,
        me as u64,
        me as u64,
        2,
        args.as_mut_ptr() as u64,
        0,
    ];
    // SAFETY: a builtin routine from the runner's table with the builtin
    // signature; `args` holds two live values (the struct borrowed, the name
    // owned here) and `result` owns nothing; `me` is the live global instance.
    let ok = unsafe { crate::hooks::guarded_call(routine, a) }.is_ok();
    let number = (ok && result.type_tag() != kind::STRING)
        .then(|| result.as_f64().map(|n| format!("{n}")))
        .flatten();
    let text = (ok && result.type_tag() == kind::STRING).then(|| {
        let mut s = StrRef {
            ptr: core::ptr::null(),
            len: 0,
        };
        // SAFETY: `result` is a live string value; `s` borrows it until released below.
        if unsafe { value_get_string(&raw const result, &raw mut s) } == Status::Ok as i32 {
            // SAFETY: valid UTF-8 or not, copied out lossily before the release.
            let bytes = unsafe { std::slice::from_raw_parts(s.ptr, s.len) };
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            String::new()
        }
    });
    // SAFETY: both values are owned here (the call's result and the name string).
    unsafe {
        release(&raw mut result);
        release(&raw mut args[1]);
    }
    text.filter(|t| !t.is_empty()).or(number)
}

/// The global instance pointer (`self` for builtins that need none).
fn global_instance_ptr() -> Option<*mut CInstance> {
    // SAFETY: host fn with no arguments.
    let p = unsafe { global_instance() };
    (!p.is_null()).then_some(p)
}

unsafe extern "C" fn guard_request(flags: u32, on: bool) -> i32 {
    if on {
        crate::guard::request(flags);
    } else {
        crate::guard::release(flags);
    }
    if !crate::guard::booted() {
        return Status::Ok as i32;
    }
    let events_ok = flags & crate::guard::EVENTS == 0 || crate::guard::events_live();
    let calls_ok = flags & crate::guard::CALLS == 0 || crate::hooks::guard_installed();
    if on && !(events_ok && calls_ok) {
        Status::NotReady as i32
    } else {
        Status::Ok as i32
    }
}

unsafe extern "C" fn guard_mark(ids: *const i32, n: usize) {
    if ids.is_null() || n == 0 {
        crate::guard::set_marked([]);
        return;
    }
    // SAFETY: the SDK passes `n` contiguous ids that live for the call.
    let ids = unsafe { std::slice::from_raw_parts(ids, n) };
    crate::guard::set_marked(ids.iter().copied());
}

unsafe extern "C" fn guard_scope(flags: u32, objects: *const i32, n: usize, kinds: u32) {
    if objects.is_null() || n == 0 {
        crate::guard::set_scope(flags, [], kinds);
    } else {
        // SAFETY: the SDK passes `n` contiguous object indices that live for
        // the call.
        let objects = unsafe { std::slice::from_raw_parts(objects, n) };
        crate::guard::set_scope(flags, objects.iter().copied(), kinds);
    }
    log::debug!(
        "GML error guard: scope set: offspring {}, {} objects, event kinds {kinds:#x}",
        if flags & crate::guard::SCOPE_OFFSPRING != 0 {
            "on"
        } else {
            "off"
        },
        crate::guard::contained_object_count()
    );
}

unsafe extern "C" fn guard_expect(on: bool) -> u32 {
    crate::guard::expect(on)
}

unsafe extern "C" fn guard_take(cb: GuardReportCallback, user: *mut c_void) -> u32 {
    let reports = crate::guard::take_reports();
    for r in &reports {
        let report = GuardReport {
            caught: u32::from(r.caught == crate::guard::Caught::Call),
            instance: r.instance,
            object: r.object,
            marked: r.why.code(),
            text: StrRef {
                ptr: r.text.as_ptr(),
                len: r.text.len(),
            },
            expected: u32::from(r.expected),
        };
        // SAFETY: SDK callback; `report` and its text live for the call.
        unsafe { cb(user, &raw const report) };
    }
    reports.len() as u32
}

unsafe extern "C" fn observe_event(
    mod_id: u32,
    kind: i32,
    number: i32,
    cb: EventObserverCallback,
    user: *mut c_void,
) -> i32 {
    let status = crate::guard::add_observer(crate::guard::Observer {
        kind,
        number,
        cb,
        user: user as usize,
        mod_id,
    });
    if status == Status::Ok {
        log::debug!(target: mod_name(mod_id), "observing events of kind {kind}, number {number}");
    } else {
        log::warn!(target: mod_name(mod_id), "event observer (kind {kind}, number {number}) refused: {status:?}");
    }
    status as i32
}

// ---- Extension functions (GameMaker extension DLLs) ----

/// Extension DLLs whose exported functions are callable by name. The runner
/// keeps extension functions in a separate table (ids from 500000, see
/// the runner's `Code_Function_Find`), not in the builtin table; their
/// routines are the DLL's exports with the builtin signature.
const EXTENSION_DLLS: &[&str] = &["Steamworks_x64.dll"];
/// Ids handed out for extension functions start here (far above the builtin
/// table and the runner's own script/extension id ranges).
const EXTENSION_ID_BASE: i32 = 0x4000_0000;

static EXTENSIONS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// The MSVC-mangled export of a GameMaker extension function `name`:
/// `void name(RValue&, CInstance*, CInstance*, int, RValue*)`, the builtin
/// routine signature.
fn extension_symbol(name: &str) -> String {
    format!("?{name}@@YAXAEAURValue@@PEAVCInstance@@1HPEAU1@@Z")
}

/// Find `name` among the extension DLLs' exports and give it an id.
fn extension_id(name: &str) -> Option<i32> {
    if !name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) {
        return None;
    }
    let sym = extension_symbol(name);
    let addr = EXTENSION_DLLS
        .iter()
        .find_map(|dll| crate::win::loaded_module_export(dll, &sym))?;
    let mut v = EXTENSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = v.iter().position(|&a| a == addr).unwrap_or_else(|| {
        v.push(addr);
        v.len() - 1
    });
    log::debug!("extension function `{name}` at {addr:#x}");
    i32::try_from(index).ok().map(|i| EXTENSION_ID_BASE + i)
}

fn extension_routine(id: i32) -> Option<usize> {
    let i = usize::try_from(id - EXTENSION_ID_BASE).ok()?;
    EXTENSIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(i)
        .copied()
}

// ---- RValue strings and release ----------------------------------------------

/// `void FREE_RValue(RValue*)`: releases the payload, does not reset the kind.
type FreeFn = unsafe extern "C-unwind" fn(*mut RValue);
/// `void YYCreateString(RValue* out, const char* s)`: releases `*out`, then
/// makes it a new string holding a copy of `s`.
type CreateStringFn = unsafe extern "C-unwind" fn(*mut RValue, *const u8);

/// `RefString`, the heap object a string `RValue` points to:
/// `{ const char* text; int32 refcount; int32 length; }`. Bit 31 of `length`
/// marks a constant (non-owned) text. As read by the runner's create-string
/// and release routines.
const REFSTRING_TEXT: usize = 0x00;
const REFSTRING_REFCOUNT: usize = 0x08;

fn rvalue_free_fn() -> Option<FreeFn> {
    let addr = index()?.rvalue_free?;
    // SAFETY: `addr` was found by `yyc::find_rvalue_free` as the routine compiled
    // code calls to release string/array/object RValues, with this signature.
    Some(unsafe { std::mem::transmute::<usize, FreeFn>(addr) })
}

static CREATE_STRING: OnceLock<Result<CreateStringFn, String>> = OnceLock::new();

/// The runner's create-string routine, found on first use (needs the builtin
/// function table, so only after the game has started).
fn create_string_fn() -> Result<CreateStringFn, Status> {
    if let Some(r) = CREATE_STRING.get() {
        return r.as_ref().copied().map_err(|_| Status::NotReady);
    }
    let table = functions()?; // NotReady before start-up: not cached
    let r = CREATE_STRING.get_or_init(|| {
        let r = discover_create_string(table);
        match &r {
            Ok(f) => log::debug!("string routines: create @ {:#x}, free ok", *f as usize),
            Err(e) => log::error!("unable to find the string routines ({e})"),
        }
        r
    });
    r.as_ref().copied().map_err(|_| Status::NotReady)
}

fn discover_create_string(table: &FunctionTable) -> Result<CreateStringFn, String> {
    let img = IMAGE.get().ok_or("image info not set")?;
    let free = rvalue_free_fn().ok_or("RValue release routine not found at boot")?;
    let routines: Vec<usize> = crate::yyc::STRING_BUILTINS
        .iter()
        .filter_map(|n| table.by_name.get(*n))
        .filter_map(|&id| table.routines.get(id as usize).copied().flatten())
        .collect();
    let image = crate::yyc::Image {
        base: img.base,
        bytes: img.bytes,
    };
    let candidates = crate::yyc::create_string_candidates(image, &img.layout, &routines);
    let &addr = candidates
        .first()
        .ok_or_else(|| format!("no candidate among callees of {} routines", routines.len()))?;
    // SAFETY: `addr` is the start of a `.text` function called by most
    // string-returning builtins and shaped like the create-string routine
    // (`yyc::looks_like_create_string`); verified by the self-test below.
    let create = unsafe { std::mem::transmute::<usize, CreateStringFn>(addr) };
    self_test_create_string(create, free)?;
    Ok(create)
}

/// Create a string, check that the runner copied it into a fresh `RefString`
/// with refcount 1, then release it.
fn self_test_create_string(create: CreateStringFn, free: FreeFn) -> Result<(), String> {
    const PROBE: &[u8] = b"ndml-selftest\0";
    let mut v = RValue::undefined();
    // SAFETY: candidate create-string routine (see caller) on a non-owning
    // RValue and a NUL-terminated buffer that outlives the call.
    unsafe { create(&raw mut v, PROBE.as_ptr()) };
    if v.type_tag() != kind::STRING || v.payload == 0 {
        return Err(format!("self-test: got kind {}", v.type_tag()));
    }
    let refstr = v.payload as usize;
    // SAFETY: a string RValue made by the runner points at a live RefString
    // (layout above); its text pointer and refcount are read.
    let (text, refcount) = unsafe {
        (
            ((refstr + REFSTRING_TEXT) as *const *const u8).read(),
            ((refstr + REFSTRING_REFCOUNT) as *const i32).read(),
        )
    };
    let copied = text != PROBE.as_ptr()
        && crate::win::read_c_str(text as usize, 64).as_deref() == Some("ndml-selftest");
    // SAFETY: release the string just created, with the runner's routine.
    unsafe { free(&raw mut v) };
    if copied && refcount == 1 {
        Ok(())
    } else {
        Err(format!("self-test: copied={copied} refcount={refcount}"))
    }
}

/// Release what `*v` owns and reset it to `undefined`.
///
/// # Safety
/// `v` must point to a valid `RValue` that the caller owns.
unsafe fn release(v: *mut RValue) -> Status {
    // SAFETY: caller contract.
    let owning = unsafe { (*v).is_owning() };
    if owning {
        let Some(free) = rvalue_free_fn() else {
            return Status::NotReady;
        };
        // SAFETY: the runner's release routine on an owned RValue.
        unsafe { free(v) };
    }
    // SAFETY: caller contract; the payload is released (or was not owned).
    unsafe { v.write(RValue::undefined()) };
    Status::Ok
}

/// [`release`] for the loader's own calls (the guard's draw-state getters).
///
/// # Safety
/// As [`release`].
pub(crate) unsafe fn release_value(v: *mut RValue) {
    // SAFETY: caller contract.
    let _ = unsafe { release(v) };
}

unsafe extern "C-unwind" fn value_free(value: *mut RValue) -> i32 {
    if value.is_null() {
        return Status::InvalidArgument as i32;
    }
    // SAFETY: SDK contract: `value` is a valid RValue it owns.
    unsafe { release(value) as i32 }
}

unsafe extern "C-unwind" fn value_set_string(out: *mut RValue, s: StrRef) -> i32 {
    if out.is_null() {
        return Status::InvalidArgument as i32;
    }
    // SAFETY: SDK passes a valid UTF-8 StrRef.
    let s = unsafe { s.as_str() };
    let Ok(c) = std::ffi::CString::new(s) else {
        return Status::InvalidArgument as i32;
    };
    let create = match create_string_fn() {
        Ok(f) => f,
        Err(e) => return e as i32,
    };
    // SAFETY: `out` is a valid RValue the SDK owns.
    let st = unsafe { release(out) };
    if st != Status::Ok {
        return st as i32;
    }
    // SAFETY: verified create-string routine; `out` owns nothing now; `c` is
    // NUL-terminated and outlives the call (the runner copies it).
    unsafe { create(out, c.as_ptr().cast()) };
    Status::Ok as i32
}

unsafe extern "C" fn value_get_string(value: *const RValue, out: *mut StrRef) -> i32 {
    if value.is_null() || out.is_null() {
        return Status::InvalidArgument as i32;
    }
    // SAFETY: SDK contract: `value` is a valid RValue.
    let v = unsafe { *value };
    if v.type_tag() != kind::STRING {
        return Status::InvalidArgument as i32;
    }
    let refstr = v.payload as usize;
    let text = if refstr == 0 {
        core::ptr::null()
    } else {
        // SAFETY: a live string RValue points at a live RefString (layout above).
        unsafe { ((refstr + REFSTRING_TEXT) as *const *const u8).read() }
    };
    let bytes: &[u8] = if text.is_null() {
        &[]
    } else {
        // SAFETY: the runner keeps the text NUL-terminated while the value lives.
        unsafe { std::ffi::CStr::from_ptr(text.cast()) }.to_bytes()
    };
    // SAFETY: `out` is a valid out-pointer (SDK contract).
    unsafe {
        out.write(StrRef {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        });
    }
    Status::Ok as i32
}

// ---- `with` (iterate instances by id or object) ----

type WithBeginFn = unsafe extern "C-unwind" fn(
    iter: *mut c_void,
    self_inst: *mut *mut CInstance,
    other_inst: *mut *mut CInstance,
    target: *mut RValue,
) -> i32;
type WithNextFn = unsafe extern "C-unwind" fn(
    iter: *mut c_void,
    self_inst: *mut *mut CInstance,
    other_inst: *mut *mut CInstance,
) -> bool;
type WithEndFn = unsafe extern "C-unwind" fn(
    iter: *mut c_void,
    self_inst: *mut *mut CInstance,
    other_inst: *mut *mut CInstance,
);

#[derive(Clone, Copy)]
struct WithFns {
    begin: WithBeginFn,
    next: WithNextFn,
    end: WithEndFn,
}

/// Room for the runner's `with` iterator. Compiled code keeps it on the stack;
/// `end` reads `+0x00`/`+0x08` (saved self/other) and frees `+0x10` (the
/// instance list). Generously sized.
const WITH_ITER_QWORDS: usize = 32;

static WITH: OnceLock<Option<WithFns>> = OnceLock::new();

fn with_fns() -> Option<WithFns> {
    *WITH.get_or_init(|| {
        let (Some(img), Some(index)) = (IMAGE.get(), index()) else {
            return None;
        };
        let image = crate::yyc::Image {
            base: img.base,
            bytes: img.bytes,
        };
        let funcs: Vec<usize> = index.functions.values().copied().collect();
        let t = std::time::Instant::now();
        let Some(r) = crate::yyc::find_with_routines(image, &img.layout, &funcs) else {
            log::error!("unable to find the `with` routines");
            return None;
        };
        log::debug!(
            "`with` routines: begin @ {:#x}, next @ {:#x}, end @ {:#x} (scan {:?})",
            r.begin - img.base,
            r.next - img.base,
            r.end - img.base,
            t.elapsed()
        );
        // SAFETY: the three addresses are the targets compiled GML code calls
        // with exactly these signatures around every `with` block
        // (`yyc::find_with_routines` votes on thousands of such call sites).
        Some(unsafe {
            WithFns {
                begin: std::mem::transmute::<usize, WithBeginFn>(r.begin),
                next: std::mem::transmute::<usize, WithNextFn>(r.next),
                end: std::mem::transmute::<usize, WithEndFn>(r.end),
            }
        })
    })
}

unsafe extern "C-unwind" fn with_instances(
    self_inst: *mut CInstance,
    other_inst: *mut CInstance,
    target: *const RValue,
    cb: WithCallback,
    user: *mut c_void,
) -> i32 {
    if self_inst.is_null() || target.is_null() {
        return -(Status::InvalidArgument as i32);
    }
    // SAFETY: the caller passes a valid RValue.
    let mut target = unsafe { *target };
    // Only what the runner's `with` accepts without raising an error.
    if !matches!(
        target.type_tag(),
        kind::REF | kind::REAL | kind::INT32 | kind::INT64
    ) {
        return -(Status::InvalidArgument as i32);
    }
    let Some(w) = with_fns() else {
        return -(Status::NotReady as i32);
    };
    let mut iter = [0u64; WITH_ITER_QWORDS];
    let it = iter.as_mut_ptr().cast::<c_void>();
    let (mut s, mut o) = (self_inst, other_inst);
    let mut count = 0i32;
    // SAFETY: the runner's own `with` protocol, as compiled code runs it: `begin`
    // fills the zeroed iterator and points `s` at the first instance (`o` at the
    // caller's self), `next` advances, `end` restores `s`/`o` and frees the list.
    // `self_inst`/`other_inst` are live (the hook's instances); `target` is a
    // plain (non-owning) ref or number; `cb` gets live instances only.
    unsafe {
        if (w.begin)(it, &raw mut s, &raw mut o, &raw mut target) > 0 {
            loop {
                cb(user, s, o);
                count += 1;
                if !(w.next)(it, &raw mut s, &raw mut o) {
                    break;
                }
            }
        }
        (w.end)(it, &raw mut s, &raw mut o);
    }
    count
}

/// Run the `with` routine scan now (boot), so the first `Call::with` in a game
/// frame doesn't pay for it. Logs the outcome.
pub fn find_with_routines() {
    let _ = with_fns();
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C-unwind" fn noop(_: *mut ndml_sdk::abi::HookCall, _: *mut c_void) {}

    /// A hook on a function the detour cannot move reaches the mod as
    /// `Unsupported` (and the loader logs `hook <name> refused: Unsupported
    /// (<why>)`): the code here starts with `loop`.
    #[test]
    fn an_unmovable_function_reaches_the_mod_as_unsupported() {
        let code = [0xE2, 0x02, 0x31, 0xC0, 0xB8, 3, 0, 0, 0, 0xC3];
        let at = crate::win::alloc_code_near(noop as *const () as usize, 64, 1 << 30)
            .expect("code memory");
        // SAFETY: fresh code memory nothing runs.
        unsafe { crate::win::write_code(at, &code) }.expect("write");
        let mut index = RunnerIndex::default();
        index.functions.insert("gml_Script_loops".to_owned(), at);
        set_index(index);
        let name = "gml_Script_loops";
        // SAFETY: a valid UTF-8 name alive for the call; the callback is
        // never called (the hook is refused).
        let status = unsafe {
            hook_gml(
                0,
                ndml_sdk::abi::StrRef::new(name),
                noop,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, Status::Unsupported as i32);
    }
}
