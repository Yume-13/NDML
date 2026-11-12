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

//! Safe wrappers over the Win32 calls the loader needs, and over calling into
//! mod DLLs. Unsafe because these are FFI calls with pointer arguments; each
//! wrapper upholds the documented contract so callers stay in safe code.

#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use ndml_sdk::abi::{MOD_ABI_SYMBOL, MOD_ENTRY_SYMBOL, ModAbiFn, ModEntryFn, ModInit};
use windows_sys::Win32::Foundation::MAX_PATH;
use windows_sys::Win32::System::Console::AllocConsole;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringA;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW,
};

use crate::pe;

/// Message to an attached debugger (e.g. `DebugView`) for failures before the log exists.
pub fn debug_out(msg: &str) {
    let mut bytes = format!("[ndml] {msg}\n").into_bytes();
    bytes.retain(|&b| b != 0);
    bytes.push(0);
    // SAFETY: NUL-terminated buffer that outlives the call.
    unsafe { OutputDebugStringA(bytes.as_ptr()) };
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
    // it is only read here.
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

/// Read a NUL-terminated string (at most `max` bytes) at an arbitrary address,
/// after checking with `VirtualQuery` that the memory is committed and readable.
/// `None` for unreadable memory, a missing terminator, or invalid UTF-8.
pub fn read_c_str(addr: usize, max: usize) -> Option<String> {
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
    let len = max.min(region_end.checked_sub(addr)?);
    // SAFETY: [addr, addr+len) lies within one committed, readable region.
    let bytes = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
    let end = bytes.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&bytes[..end]).ok().map(str::to_owned)
}
