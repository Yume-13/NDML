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

//! Safe wrappers over the Win32 calls the loader needs, and over calling into
//! mod DLLs. Unsafe because these are FFI calls with pointer arguments; each
//! wrapper upholds the documented contract so callers stay in safe code.

#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ndml_sdk::abi::{MOD_ABI_SYMBOL, MOD_ENTRY_SYMBOL, ModAbiFn, ModEntryFn, ModInit};
use windows_sys::Win32::Foundation::{MAX_PATH, SYSTEMTIME};
use windows_sys::Win32::System::Console::AllocConsole;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringA;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

use crate::pe;

/// Message to an attached debugger (e.g. `DebugView`) for failures before the log exists.
pub fn debug_out(msg: &str) {
    let mut bytes = format!("[ndml] {msg}\n").into_bytes();
    bytes.retain(|&b| b != 0);
    bytes.push(0);
    // SAFETY: NUL-terminated buffer that outlives the call.
    unsafe { OutputDebugStringA(bytes.as_ptr()) };
}

/// The PC's local date and time now (`GetLocalTime`), as plain fields.
#[derive(Debug, Clone, Copy)]
pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

/// The PC's local date and time now (the kept logs' names).
pub fn local_time() -> LocalTime {
    let mut t = SYSTEMTIME::default();
    // SAFETY: `t` is a valid, writable SYSTEMTIME for the whole call;
    // GetLocalTime only fills it and cannot fail.
    unsafe { GetLocalTime(&raw mut t) };
    LocalTime {
        year: t.wYear,
        month: t.wMonth,
        day: t.wDay,
        hour: t.wHour,
        minute: t.wMinute,
        second: t.wSecond,
    }
}

/// Full path of the host executable.
pub fn exe_path() -> Option<PathBuf> {
    let mut buf = [0u16; MAX_PATH as usize * 4];
    // SAFETY: buffer and its length are passed correctly; null module = the EXE.
    let n =
        unsafe { GetModuleFileNameW(core::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    (n > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
}

/// The host executable as mapped in memory.
#[derive(Debug)]
pub struct MainImage {
    pub base: usize,
    pub layout: pe::Layout,
    /// The whole image (`SizeOfImage` bytes from `base`).
    pub bytes: &'static [u8],
}

/// Map view of the host executable. `None` if its headers can't be parsed.
pub fn main_image() -> Option<MainImage> {
    // SAFETY: a null name returns the EXE's module handle, valid for the process lifetime.
    let base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
    if base == 0 {
        return None;
    }
    // SAFETY: the PE headers (at least one page) are mapped readable at the module base.
    let headers = unsafe { std::slice::from_raw_parts(base as *const u8, 4096) };
    let layout = pe::parse(headers).ok()?;
    // SAFETY: the loader maps the full SizeOfImage range for the process lifetime;
    // it is only read.
    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, layout.size_of_image) };
    Some(MainImage {
        base,
        layout,
        bytes,
    })
}

/// Open a console window for the process (no-op if one already exists).
pub fn alloc_console() {
    // SAFETY: no arguments; failure (console already present) is harmless.
    unsafe { AllocConsole() };
}

/// A loaded mod DLL with its two required exports.
#[derive(Debug)]
pub struct ModLibrary {
    abi: ModAbiFn,
    entry: ModEntryFn,
}

impl ModLibrary {
    /// Load `path` and resolve `ndml_mod_abi_version` / `ndml_mod_main`.
    /// The DLL stays loaded for the process lifetime (its hooks live in it).
    pub fn load(path: &Path) -> Result<Self, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: NUL-terminated wide path. Only called from the entry-point hook,
        // outside the loader lock, so LoadLibrary is allowed.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            return Err(format!(
                "LoadLibrary failed ({})",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: valid module handle and NUL-terminated symbol names.
        let (abi, entry) = unsafe {
            (
                GetProcAddress(module, MOD_ABI_SYMBOL.as_ptr()),
                GetProcAddress(module, MOD_ENTRY_SYMBOL.as_ptr()),
            )
        };
        let (Some(abi), Some(entry)) = (abi, entry) else {
            return Err("not an NDML mod (missing ndml_mod_main / ndml_mod_abi_version)".into());
        };
        // SAFETY: these exports are generated by `ndml_sdk::export_mod!` with exactly
        // these signatures.
        let (abi, entry) = unsafe {
            (
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ModAbiFn>(abi),
                std::mem::transmute::<unsafe extern "system" fn() -> isize, ModEntryFn>(entry),
            )
        };
        Ok(Self { abi, entry })
    }

    /// The ABI version the mod was built against.
    pub fn abi_version(&self) -> u32 {
        // SAFETY: export contract; takes no arguments.
        unsafe { (self.abi)() }
    }

    /// Run the mod's init. Returns its status code (0 = ok).
    pub fn init(&self, init: &ModInit) -> i32 {
        // SAFETY: `init` is valid for the call and its strings/host table are
        // 'static, as the SDK requires.
        unsafe { (self.entry)(init) }
    }
}

/// Copy `len` bytes of the host executable's mapped image starting at absolute
/// address `addr`. The range must lie inside the image (`MainImage` bounds);
/// returns `None` otherwise. Used to snapshot runner tables in `.data` at
/// runtime, after the runner has filled them.
pub fn copy_image_range(
    image_base: usize,
    size_of_image: usize,
    addr: usize,
    len: usize,
) -> Option<Vec<u8>> {
    let end = addr.checked_add(len)?;
    if addr < image_base || end > image_base.checked_add(size_of_image)? {
        return None;
    }
    let mut out = vec![0u8; len];
    // SAFETY: the range is inside the EXE image, which is mapped (readable) for
    // the process lifetime; `out` has `len` bytes. Concurrent writes by game
    // threads can only make the snapshot stale, never invalid as bytes.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), len) };
    Some(out)
}

/// Number of bytes readable from `addr` to the end of its memory region, after
/// checking with `VirtualQuery` that the region is committed, readable and not
/// a guard page. `None` if `addr` is null or unreadable.
fn readable_len(addr: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        PAGE_WRITECOPY, VirtualQuery,
    };
    if addr == 0 {
        return None;
    }
    // SAFETY: MEMORY_BASIC_INFORMATION is plain data; all-zero is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: VirtualQuery only inspects the address space; buffer and size match.
    let n = unsafe {
        VirtualQuery(
            addr as *const core::ffi::c_void,
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || info.State != MEM_COMMIT || info.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 {
        return None;
    }
    let readable = PAGE_READONLY
        | PAGE_READWRITE
        | PAGE_WRITECOPY
        | PAGE_EXECUTE_READ
        | PAGE_EXECUTE_READWRITE
        | PAGE_EXECUTE_WRITECOPY;
    if info.Protect & readable == 0 {
        return None;
    }
    let region_end = (info.BaseAddress as usize).checked_add(info.RegionSize)?;
    region_end.checked_sub(addr).filter(|&n| n > 0)
}

/// Read a NUL-terminated string (at most `max` bytes) at an arbitrary address,
/// after checking with `VirtualQuery` that the memory is committed and readable.
/// `None` for unreadable memory, a missing terminator, or invalid UTF-8.
pub fn read_c_str(addr: usize, max: usize) -> Option<String> {
    let len = max.min(readable_len(addr)?);
    // SAFETY: [addr, addr+len) lies within one committed, readable region.
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let end = bytes.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned)
}

/// Copy `len` bytes at an arbitrary address (e.g. a runner table on the heap),
/// checking every memory region it spans with `VirtualQuery` first. `None` if
/// any part is unreadable. The copy may be stale if a game thread writes
/// concurrently, but is never invalid as bytes.
pub fn read_memory(addr: usize, len: usize) -> Option<Vec<u8>> {
    let end = addr.checked_add(len)?;
    let mut at = addr;
    while at < end {
        at = at.checked_add(readable_len(at)?)?;
    }
    let mut out = vec![0u8; len];
    // SAFETY: every byte of [addr, addr+len) was just checked to be in a
    // committed, readable region; `out` has `len` bytes.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), len) };
    Some(out)
}

// ---- Code memory for inline detours (`crate::hooks::Detour`) ----

/// `VirtualQuery` of `addr`, or `None` past the address space.
fn query(addr: usize) -> Option<windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION> {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: MEMORY_BASIC_INFORMATION is plain data; all-zero is valid.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: VirtualQuery only inspects the address space; buffer and size match.
    let n = unsafe {
        VirtualQuery(
            addr as *const core::ffi::c_void,
            &raw mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    (n != 0).then_some(info)
}

/// Up to `max` bytes of code at `addr`, from its memory region only (fewer
/// at the region's end). `None` unless the region is committed and
/// executable.
pub fn read_code(addr: usize, max: usize) -> Option<Vec<u8>> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
    };
    let info = query(addr)?;
    let readable_code = PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    if info.State != MEM_COMMIT
        || info.Protect & (PAGE_GUARD | PAGE_NOACCESS | PAGE_EXECUTE) != 0
        || info.Protect & readable_code == 0
    {
        return None;
    }
    let end = (info.BaseAddress as usize).checked_add(info.RegionSize)?;
    let len = max.min(end.checked_sub(addr)?);
    let mut out = vec![0u8; len];
    // SAFETY: [addr, addr+len) lies in one committed, readable region; `out`
    // has `len` bytes.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), len) };
    Some(out)
}

/// One allocation of code memory: relays and trampolines, never freed.
const CODE_BLOCK: usize = 0x1_0000;
/// Windows' allocation granularity on x64 (where `VirtualAlloc` can place a
/// block).
const ALLOC_GRANULARITY: usize = 0x1_0000;

#[derive(Debug)]
struct CodeBlock {
    base: usize,
    used: usize,
}

static CODE_BLOCKS: std::sync::Mutex<Vec<CodeBlock>> = std::sync::Mutex::new(Vec::new());
/// Held by [`write_code`] from protect to flush: one code writer at a time.
static CODE_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `len` bytes (16-byte aligned) of executable memory lying wholly within
/// `near` bytes of `target`, kept for the process lifetime. Read and execute
/// only: written through [`write_code`]. `None` if no free address range is
/// within reach.
pub fn alloc_code_near(target: usize, len: usize, near: usize) -> Option<usize> {
    let len = len.next_multiple_of(16);
    if len > CODE_BLOCK {
        return None;
    }
    let within = |a: usize| a.abs_diff(target) <= near && (a + len).abs_diff(target) <= near;
    let mut blocks = CODE_BLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(b) = blocks
        .iter_mut()
        .find(|b| b.used + len <= CODE_BLOCK && within(b.base + b.used))
    {
        let at = b.base + b.used;
        b.used += len;
        return Some(at);
    }
    let base = reserve_near(target, near)?;
    blocks.push(CodeBlock { base, used: len });
    Some(base)
}

/// Commit a [`CODE_BLOCK`] in a free range within `near` of `target`: the
/// nearest free range above it first, then below.
fn reserve_near(target: usize, near: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_FREE, MEM_RESERVE, PAGE_EXECUTE_READ, VirtualAlloc,
    };
    let try_at = |a: usize| -> Option<usize> {
        // SAFETY: asks for fresh pages at a free address; no existing memory
        // is touched (the call fails if the range is taken).
        let p = unsafe {
            VirtualAlloc(
                a as *const core::ffi::c_void,
                CODE_BLOCK,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READ,
            )
        };
        (!p.is_null()).then_some(p as usize)
    };
    let lo = target.saturating_sub(near).max(ALLOC_GRANULARITY);
    let hi = target.saturating_add(near);
    let mut at = target.next_multiple_of(ALLOC_GRANULARITY);
    while at.checked_add(CODE_BLOCK)? <= hi {
        let Some(info) = query(at) else { break };
        let end = (info.BaseAddress as usize).checked_add(info.RegionSize)?;
        if info.State == MEM_FREE
            && at + CODE_BLOCK <= end
            && let Some(p) = try_at(at)
        {
            return Some(p);
        }
        at = end.next_multiple_of(ALLOC_GRANULARITY);
    }
    let mut at = (target / ALLOC_GRANULARITY * ALLOC_GRANULARITY).checked_sub(CODE_BLOCK)?;
    while at >= lo {
        let Some(info) = query(at) else { break };
        let base = info.BaseAddress as usize;
        let end = base.checked_add(info.RegionSize)?;
        if info.State == MEM_FREE
            && at + CODE_BLOCK <= end
            && let Some(p) = try_at(at)
        {
            return Some(p);
        }
        at = (base / ALLOC_GRANULARITY * ALLOC_GRANULARITY).checked_sub(CODE_BLOCK)?;
    }
    None
}

/// Write `bytes` over code at `addr`: the range made writable for the copy,
/// its protection put back, the instruction cache flushed. Writers are
/// serialised (one at a time in the process, [`CODE_WRITER`]), so two threads
/// writing code on the same page cannot undo each other's protection change.
///
/// # Safety
/// `addr..addr + bytes.len()` must be committed code that no thread executes
/// while it is written (memory from [`alloc_code_near`] nothing jumps to
/// yet, or a function no game thread runs yet), and the result must be valid
/// code.
pub unsafe fn write_code(addr: usize, bytes: &[u8]) -> Result<(), String> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let ptr = addr as *mut core::ffi::c_void;
    let mut old = 0;
    let _one_writer = CODE_WRITER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: changes only the protection of committed pages (caller).
    if unsafe { VirtualProtect(ptr, bytes.len(), PAGE_EXECUTE_READWRITE, &raw mut old) } == 0 {
        return Err(format!(
            "VirtualProtect failed ({})",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the range was just made writable; nothing executes it during
    // the copy (caller); `bytes` does not overlap code memory.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len()) };
    let mut was = 0;
    // SAFETY: puts back the protection read above on the same range.
    unsafe { VirtualProtect(ptr, bytes.len(), old, &raw mut was) };
    // SAFETY: flushes this process's instruction cache for the range.
    unsafe { FlushInstructionCache(GetCurrentProcess(), ptr, bytes.len()) };
    Ok(())
}

/// Whether key `vk` (virtual-key code) is held down while one of this
/// process's windows is in the foreground; `false` otherwise.
pub fn key_down(vk: u32) -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    let Ok(vk) = i32::try_from(vk) else {
        return false;
    };
    // SAFETY: no arguments; returns null if there is no foreground window.
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_null() {
        return false;
    }
    let mut pid = 0u32;
    // SAFETY: `fg` is a window handle from the system; `pid` outlives the call.
    unsafe { GetWindowThreadProcessId(fg, &raw mut pid) };
    // SAFETY: no arguments.
    if pid != unsafe { GetCurrentProcessId() } {
        return false;
    }
    // SAFETY: plain integer argument. The high bit of the result means "down".
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// Address of `symbol` exported by the already-loaded module `dll` (a file
/// name such as `Steamworks_x64.dll`). Never loads the DLL: `None` if the game
/// hasn't loaded it or it has no such export.
pub fn loaded_module_export(dll: &str, symbol: &str) -> Option<usize> {
    let wide: Vec<u16> = std::ffi::OsStr::new(dll)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let sym: Vec<u8> = symbol.bytes().chain(Some(0)).collect();
    if sym[..sym.len() - 1].contains(&0) {
        return None;
    }
    // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the call;
    // GetModuleHandleW only looks up an already-loaded module (no refcount).
    let module = unsafe { GetModuleHandleW(wide.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: `module` is a loaded module handle; `sym` is NUL-terminated ASCII.
    let f = unsafe { GetProcAddress(module, sym.as_ptr()) }?;
    Some(f as usize)
}

// ---- GML error guard: an SEH frame around a call into game code ----
//
// `ndml_guard_call(target, args, ret, frame)` calls `target` with six integer
// arguments (`args[0..4]` in rcx/rdx/r8/r9, `args[4..6]` on the stack) inside a
// `__try`/`__except` scope handled by the CRT's `__C_specific_handler`, as MSVC
// emits for C. Its filter, `ndml_guard_filter`, runs in the search phase while
// the thrower's frames still exist; if the frame's `decide` takes the
// exception, the handler unwinds every frame in between (the runner's own
// cleanups run, as for a C++ `catch`) and resumes at the landing label, which
// returns 1. A normal return stores rax in `*ret` and returns 0.
//
// The frame has no frame pointer: after the prologue rsp is the establisher
// frame the filter receives, so `[establisher + 0x38]` is the `GuardFrame`.
// The thunk keeps nothing in non-volatile registers, so the unwound context
// the landing resumes with is the caller's.
core::arch::global_asm!(
    ".text",
    ".p2align 4",
    ".globl ndml_guard_call",
    ".def ndml_guard_call; .scl 2; .type 32; .endef",
    "ndml_guard_call:",
    ".seh_proc ndml_guard_call",
    ".seh_handler __C_specific_handler, @except",
    "sub rsp, 0x48",
    ".seh_stackalloc 0x48",
    ".seh_endprologue",
    "mov qword ptr [rsp + 0x30], r8",
    "mov qword ptr [rsp + 0x38], r9",
    "mov rax, rcx",
    "mov r10, rdx",
    "mov rcx, qword ptr [r10 + 0x20]",
    "mov qword ptr [rsp + 0x20], rcx",
    "mov rcx, qword ptr [r10 + 0x28]",
    "mov qword ptr [rsp + 0x28], rcx",
    "mov rcx, qword ptr [r10]",
    "mov rdx, qword ptr [r10 + 0x8]",
    "mov r8, qword ptr [r10 + 0x10]",
    "mov r9, qword ptr [r10 + 0x18]",
    "ndml_guard_try_begin:",
    "call rax",
    "nop",
    "ndml_guard_try_end:",
    "mov rcx, qword ptr [rsp + 0x30]",
    "mov qword ptr [rcx], rax",
    "xor eax, eax",
    "add rsp, 0x48",
    "ret",
    "ndml_guard_landing:",
    "mov eax, 1",
    "add rsp, 0x48",
    "ret",
    ".seh_handlerdata",
    ".long 1",
    ".long ndml_guard_try_begin@IMGREL",
    ".long ndml_guard_try_end@IMGREL",
    ".long ndml_guard_filter@IMGREL",
    ".long ndml_guard_landing@IMGREL",
    ".text",
    ".seh_endproc",
);

unsafe extern "C" {
    fn ndml_guard_call(
        target: usize,
        args: *const [u64; 6],
        ret: *mut u64,
        frame: *mut core::ffi::c_void, // a `GuardFrame`
    ) -> u32;
}

/// Read by the filter through the thunk's stack slot.
#[repr(C)]
struct GuardFrame {
    decide: fn(&crate::guard::Thrown, usize) -> bool,
    arg: usize,
    /// First 16 bytes of the thrown object when taken (a `YYGMLException` is
    /// an `RValue`).
    taken: [u64; 2],
}

/// `EXCEPTION_EXECUTE_HANDLER` / `EXCEPTION_CONTINUE_SEARCH`.
const EXECUTE_HANDLER: i32 = 1;
const CONTINUE_SEARCH: i32 = 0;
/// MSVC C++ exception code (`'msc' | 0xE0000000`).
const CXX_EXCEPTION: u32 = 0xE06D_7363;

#[unsafe(no_mangle)]
extern "system" fn ndml_guard_filter(
    ptrs: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    establisher: usize,
) -> i32 {
    if ptrs.is_null() || establisher == 0 {
        return CONTINUE_SEARCH;
    }
    // SAFETY: the dispatcher passes valid exception pointers for the duration
    // of the filter call.
    let rec = unsafe { (*ptrs).ExceptionRecord };
    if rec.is_null() {
        return CONTINUE_SEARCH;
    }
    // SAFETY: as above; the record is valid while the filter runs.
    let rec = unsafe { &*rec };
    // SAFETY: `ndml_guard_call` stored its `GuardFrame` pointer at
    // `[rsp + 0x38]` after its prologue, and the establisher frame of a frame
    // without a frame register is exactly that rsp. The frame outlives the call.
    let frame = unsafe { &mut **((establisher + 0x38) as *const *mut GuardFrame) };
    let n = (rec.NumberParameters as usize).min(rec.ExceptionInformation.len());
    let mut params = [0usize; 4];
    for (p, v) in params.iter_mut().zip(&rec.ExceptionInformation[..n]) {
        *p = *v;
    }
    let thrown = crate::guard::Thrown {
        code: rec.ExceptionCode as u32,
        params,
        nparams: n,
    };
    if thrown.code != CXX_EXCEPTION || thrown.params[1] == 0 {
        return CONTINUE_SEARCH;
    }
    let (decide, arg) = (frame.decide, frame.arg);
    let take = std::panic::catch_unwind(|| decide(&thrown, arg)).unwrap_or(false);
    if !take {
        return CONTINUE_SEARCH;
    }
    // SAFETY: for an MSVC C++ exception `params[1]` is the thrown object, alive
    // in the thrower's frame until the unwind that follows this filter; `decide`
    // checked it is a `YYGMLException` (a 16-byte `RValue`).
    frame.taken = unsafe { (thrown.params[1] as *const [u64; 2]).read_unaligned() };
    EXECUTE_HANDLER
}

/// Call game function `target` with up to six integer arguments inside the
/// guard frame. `Ok(rax)` on a normal return; `Err(thrown)` when `decide`
/// (called in the filter with `arg`) took a GML error, the frames in between
/// unwound.
///
/// # Safety
/// `target` must be a function taking (up to) six integer/pointer arguments
/// in the Win64 convention, and `args` valid arguments for it. Only call from
/// the game thread.
pub unsafe fn guard_call(
    target: usize,
    args: [u64; 6],
    decide: fn(&crate::guard::Thrown, usize) -> bool,
    arg: usize,
) -> Result<u64, [u64; 2]> {
    let mut frame = GuardFrame {
        decide,
        arg,
        taken: [0; 2],
    };
    let mut ret = 0u64;
    // SAFETY: caller contract for `target`/`args`; `ret` and `frame` outlive
    // the call (the thunk only reaches them through its own stack slots).
    let caught = unsafe {
        ndml_guard_call(
            target,
            &raw const args,
            &raw mut ret,
            (&raw mut frame).cast(),
        )
    };
    if caught == 0 {
        Ok(ret)
    } else {
        Err(frame.taken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Six = extern "C-unwind" fn(u64, u64, u64, u64, u64, u64) -> u64;

    fn six(f: Six) -> usize {
        f as usize
    }

    extern "C-unwind" fn sum6(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
        a1 + 2 * a2 + 3 * a3 + 4 * a4 + 5 * a5 + 6 * a6
    }

    /// A fake image holding the MSVC throw info of `YYGMLException`.
    fn fake_image() -> (usize, &'static [u8]) {
        let mut img = vec![0u8; 0x500];
        let put = |img: &mut Vec<u8>, at: usize, v: u32| {
            img[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        put(&mut img, 0x100 + 12, 0x200); // ThrowInfo.pCatchableTypeArray
        put(&mut img, 0x200, 1); // count
        put(&mut img, 0x204, 0x300); // CatchableType
        put(&mut img, 0x304, 0x400); // .pType
        let name = b".?AVYYGMLException@@\x00";
        img[0x410..0x410 + name.len()].copy_from_slice(name);
        let img: &'static [u8] = Box::leak(img.into_boxed_slice());
        (img.as_ptr() as usize, img)
    }

    static OBJ: [u64; 2] = [0x1234, 6 << 32];

    extern "C-unwind" fn thrower(base: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> u64 {
        let params = [
            0x1993_0520usize,
            OBJ.as_ptr() as usize,
            base as usize + 0x100,
            base as usize,
        ];
        // SAFETY: raising an exception with four parameters, as MSVC's
        // `_CxxThrowException` does; the guard frame above takes it.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::RaiseException(
                CXX_EXCEPTION,
                1,
                4,
                params.as_ptr(),
            );
        }
        7
    }

    #[test]
    fn guard_frame_calls_and_catches() {
        let (base, img) = fake_image();
        crate::guard::set_image(base, img);
        // SAFETY: `sum6` takes six integers.
        let r = unsafe { guard_call(six(sum6), [1, 2, 3, 4, 5, 6], crate::guard::decide_call, 0) };
        assert_eq!(r, Ok(1 + 4 + 9 + 16 + 25 + 36));
        // SAFETY: `thrower` takes six integers and raises a GML-shaped exception.
        let r = unsafe {
            guard_call(
                six(thrower),
                [base as u64, 0, 0, 0, 0, 0],
                crate::guard::decide_call,
                0,
            )
        };
        assert_eq!(r, Err(OBJ));
        // Still usable afterwards.
        // SAFETY: as above.
        let r = unsafe { guard_call(six(sum6), [1, 0, 0, 0, 0, 0], crate::guard::decide_call, 0) };
        assert_eq!(r, Ok(1));
    }
}
