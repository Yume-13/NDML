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

//! Process entry: `DllMain` and the detour on the game's entry point.
//!
//! Unsafe because Windows calls `DllMain` by symbol name and the EXE's entry
//! point is patched. `DllMain` runs under the loader lock, so it only installs the
//! entry detour; real work happens in [`crate::bootstrap`] once the lock is
//! released and before any game code has run.

#![allow(unsafe_code)]

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use retour::RawDetour;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

use crate::win;

/// Searched for by `ndml` (the manager) to recognise the NDML `version.dll`.
#[used]
#[unsafe(no_mangle)]
pub static NDML_LOADER_MARKER: [u8; 24] = *b"NDML-LOADER-MARKER-v0.1\0";

type EntryFn = unsafe extern "system" fn(*mut c_void) -> u32;
static ENTRY_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: this module's own handle, passed in by Windows.
        unsafe { DisableThreadLibraryCalls(module) };
        let is_game = win::exe_path()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .is_some_and(|f| f.eq_ignore_ascii_case(ndml_common::GAME_EXE));
        if is_game {
            if let Err(e) = catch_unwind(install_entry_hook).unwrap_or(Err("panic".into())) {
                win::debug_out(&format!("entry hook failed, running unmodded: {e}"));
            }
        }
    }
    1
}

fn install_entry_hook() -> Result<(), String> {
    let image = win::main_image().ok_or("cannot read EXE headers")?;
    let entry = image.base + image.layout.entry_rva;
    // SAFETY: `entry` is the EXE's entry point, which has not run yet; the
    // detour has the same signature.
    let detour = unsafe { RawDetour::new(entry as *const (), entry_detour as *const ()) }
        .map_err(|e| e.to_string())?;
    ENTRY_TRAMPOLINE.store(
        std::ptr::from_ref(detour.trampoline()) as usize,
        Ordering::Release,
    );
    // SAFETY: process attach is single-threaded with respect to the EXE's code.
    unsafe { detour.enable() }.map_err(|e| e.to_string())?;
    std::mem::forget(detour); // the trampoline must stay valid forever
    Ok(())
}

unsafe extern "system" fn entry_detour(arg: *mut c_void) -> u32 {
    if catch_unwind(AssertUnwindSafe(crate::bootstrap)).is_err() {
        log::error!("loader panicked during bootstrap; continuing unmodded where possible");
    }
    log::logger().flush();
    // SAFETY: stored before the detour was enabled; it is the original entry point.
    let original: EntryFn =
        unsafe { std::mem::transmute(ENTRY_TRAMPOLINE.load(Ordering::Acquire)) };
    // SAFETY: calling the real entry point with Windows' argument.
    unsafe { original(arg) }
}
