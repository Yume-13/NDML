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

//! # ndml-loader
//!
//! Deployed into the game folder as `version.dll`. Windows loads it before the
//! game's own code runs. Boot sequence (see `docs/how-it-works.md`):
//!
//! 1. `DllMain` (loader lock held): only installs a detour on the EXE's entry
//!    point. No file I/O, no `LoadLibrary`.
//! 2. Entry-point detour (loader lock released, game code not started):
//!    settings → logging → scan YYC tables → load mods (they register hooks)
//!    → enable hook detours → jump to the real entry point.
//!
//! Any failure is logged and the game starts unmodded; a loader error never
//! prevents the game from launching.
//!
//! ## Unsafe code policy
//! Denied crate-wide. Exactly five modules opt out, each because it *is* an FFI
//! boundary: `entry` (`DllMain` + entry detour), `win` (Win32 calls, calling into
//! mod DLLs), `hooks` (detours + thunks), `host` (C functions handed to mods),
//! `proxy` (`version.dll` forwarding). Everything else is `forbid(unsafe_code)`.

#![deny(unsafe_code)]

mod entry;
pub mod hooks;
pub mod host;
pub mod logging;
pub mod mods;
pub mod pe;
mod proxy;
mod win;
pub mod yyc;

use std::path::PathBuf;

/// Runs once from the entry-point detour, before the game's own code.
fn bootstrap() {
    let Some(game_dir) = win::exe_path().and_then(|p| p.parent().map(PathBuf::from)) else {
        win::debug_out("cannot determine game directory");
        return;
    };
    let paths = ndml_common::Paths::new(&game_dir);
    if let Err(e) = paths.create_all() {
        win::debug_out(&format!("cannot create {}: {e}", paths.root.display()));
        return;
    }
    let (settings, settings_err) = ndml_common::load(&paths.settings_file());
    logging::init(&paths.log_file(), settings.level_filter(), settings.console);
    log::info!(
        "NDML {} starting in {}",
        env!("CARGO_PKG_VERSION"),
        game_dir.display()
    );
    if let Some(e) = settings_err {
        log::warn!("settings: {e} (using defaults)");
    }

    let Some(image) = win::main_image() else {
        log::error!("cannot read EXE headers");
        return;
    };
    let started = std::time::Instant::now();
    let index = match yyc::scan(
        yyc::Image {
            base: image.base,
            bytes: image.bytes,
        },
        &image.layout,
    ) {
        Ok(i) => i,
        Err(e) => {
            log::error!("runner scan failed: {e}; mods disabled");
            return;
        }
    };
    log::info!(
        "runner index: {} functions, {} variables ({} duplicate names) in {:?}",
        index.functions.len(),
        index.variables.len(),
        index.duplicates,
        started.elapsed()
    );
    if index.global_instance_ptr.is_none() {
        log::warn!("global instance pointer not found; mods can't read globals");
    }
    host::set_index(index);
    if let (Some(text), Some(data)) = (image.layout.section(".text"), image.layout.section(".data"))
    {
        host::set_image(host::ImageInfo {
            base: image.base,
            size: image.layout.size_of_image,
            text: image.base + text.rva..image.base + text.rva + text.size,
            data: image.base + data.rva..image.base + data.rva + data.size,
        });
    }

    mods::load_all(&paths, &settings);
    match hooks::install() {
        Ok(n) => log::info!("{n} hook site(s) active"),
        Err(e) => log::error!("hook install failed: {e}"),
    }
}
