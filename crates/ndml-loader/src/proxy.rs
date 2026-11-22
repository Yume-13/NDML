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

//! `version.dll` proxy: every export forwards to the real system DLL.
//!
//! The game imports `GetFileVersionInfoW`, `GetFileVersionInfoSizeW` and
//! `VerQueryValueW`; the whole documented API is forwarded so anything else in
//! the process that binds to "version.dll" keeps working. The real DLL is
//! loaded lazily on first use (never from `DllMain`).

#![allow(unsafe_code)] // forwards raw Win32 calls to the system DLL

use core::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

static REAL: AtomicUsize = AtomicUsize::new(0);

fn real_module() -> usize {
    let m = REAL.load(Ordering::Acquire);
    if m != 0 {
        return m;
    }
    let mut buf = [0u16; 260];
    // SAFETY: buffer length passed correctly.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let mut path: Vec<u16> = buf[..n.min(buf.len())].to_vec();
    path.extend("\\version.dll\0".encode_utf16());
    // SAFETY: NUL-terminated absolute path to the system DLL.
    let m = unsafe { LoadLibraryW(path.as_ptr()) } as usize;
    REAL.store(m, Ordering::Release);
    m
}

fn resolve(cache: &AtomicUsize, name: &[u8]) -> usize {
    let f = cache.load(Ordering::Acquire);
    if f != 0 {
        return f;
    }
    let m = real_module();
    if m == 0 {
        return 0;
    }
    // SAFETY: valid module handle; `name` is NUL-terminated.
    let f = unsafe { GetProcAddress(m as _, name.as_ptr()) }.map_or(0, |f| f as usize);
    cache.store(f, Ordering::Release);
    f
}

macro_rules! forward {
    ($( fn $name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty; )*) => {$(
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            static CACHE: AtomicUsize = AtomicUsize::new(0);
            let f = resolve(&CACHE, concat!(stringify!($name), "\0").as_bytes());
            if f == 0 {
                return 0;
            }
            // SAFETY: the real export has exactly this signature.
            let f: unsafe extern "system" fn($($ty),*) -> $ret = unsafe { std::mem::transmute(f) };
            // SAFETY: forwarding the caller's arguments unchanged.
            unsafe { f($($arg),*) }
        }
    )*};
}

forward! {
    fn GetFileVersionInfoA(file: *const u8, handle: u32, len: u32, data: *mut c_void) -> i32;
    fn GetFileVersionInfoW(file: *const u16, handle: u32, len: u32, data: *mut c_void) -> i32;
    fn GetFileVersionInfoExA(flags: u32, file: *const u8, handle: u32, len: u32, data: *mut c_void) -> i32;
    fn GetFileVersionInfoExW(flags: u32, file: *const u16, handle: u32, len: u32, data: *mut c_void) -> i32;
    fn GetFileVersionInfoSizeA(file: *const u8, handle: *mut u32) -> u32;
    fn GetFileVersionInfoSizeW(file: *const u16, handle: *mut u32) -> u32;
    fn GetFileVersionInfoSizeExA(flags: u32, file: *const u8, handle: *mut u32) -> u32;
    fn GetFileVersionInfoSizeExW(flags: u32, file: *const u16, handle: *mut u32) -> u32;
    fn VerFindFileA(flags: u32, file: *const u8, win_dir: *const u8, app_dir: *const u8,
        cur_dir: *mut u8, cur_len: *mut u32, dest_dir: *mut u8, dest_len: *mut u32) -> u32;
    fn VerFindFileW(flags: u32, file: *const u16, win_dir: *const u16, app_dir: *const u16,
        cur_dir: *mut u16, cur_len: *mut u32, dest_dir: *mut u16, dest_len: *mut u32) -> u32;
    fn VerInstallFileA(flags: u32, src_file: *const u8, dest_file: *const u8, src_dir: *const u8,
        dest_dir: *const u8, cur_dir: *const u8, tmp_file: *mut u8, tmp_len: *mut u32) -> u32;
    fn VerInstallFileW(flags: u32, src_file: *const u16, dest_file: *const u16, src_dir: *const u16,
        dest_dir: *const u16, cur_dir: *const u16, tmp_file: *mut u16, tmp_len: *mut u32) -> u32;
    fn VerLanguageNameA(lang: u32, buf: *mut u8, len: u32) -> u32;
    fn VerLanguageNameW(lang: u32, buf: *mut u16, len: u32) -> u32;
    fn VerQueryValueA(block: *const c_void, sub_block: *const u8, out: *mut *mut c_void, len: *mut u32) -> i32;
    fn VerQueryValueW(block: *const c_void, sub_block: *const u16, out: *mut *mut c_void, len: *mut u32) -> i32;
}
