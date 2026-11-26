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
//! prevents the game from launching. A failure before the log is opened (the
//! entry detour, the `ndml/` folder) goes only to the debugger output
//! (`OutputDebugString`), and `ndml.log` stays the previous session's.
//!
//! ## Unsafe code policy
//! Denied crate-wide. Exactly five modules opt out, each because it *is* an FFI
//! boundary: `entry` (`DllMain` + entry detour), `win` (Win32 calls, calling into
//! mod DLLs), `hooks` (detours + thunks), `host` (C functions handed to mods),
//! `proxy` (`version.dll` forwarding). Everything else is `forbid(unsafe_code)`.

#![deny(unsafe_code)]

pub mod detour;
mod entry;
pub mod guard;
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
    let (settings, settings_problems) = ndml_common::load(&paths.settings_file());
    let log_problems = logging::init(&paths.logs, settings.level_filter(), settings.console);
    log::info!(
        "NDML {} starting in {}",
        env!("CARGO_PKG_VERSION"),
        game_dir.display()
    );
    if !log_problems.is_empty() {
        log::warn!("keeping the last logs: {}", log_problems.join("; "));
    }
    // The whole file unreadable ("unable to read ndml.toml (...); using
    // defaults"), or one line per key skipped or left at its default.
    for p in settings_problems {
        log::warn!("{p}");
    }

    let Some(image) = win::main_image() else {
        log::error!("unable to read the game's EXE headers");
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
            log::error!("unable to index the game ({e}); mods not loaded");
            return;
        }
    };
    log::debug!(
        "runner index: {} functions, {} variables ({} duplicate names) in {:?}",
        index.functions.len(),
        index.variables.len(),
        index.duplicates,
        started.elapsed()
    );
    if index.global_instance_ptr.is_none() {
        log::warn!("unable to find the global instance");
    }
    match index.rvalue_free {
        Some(a) => log::debug!("RValue release routine at {:#x}", a - image.base),
        None => log::warn!("unable to find the value release routine"),
    }
    host::set_index(index);
    if let (Some(text), Some(data)) = (image.layout.section(".text"), image.layout.section(".data"))
    {
        host::set_image(host::ImageInfo {
            base: image.base,
            size: image.layout.size_of_image,
            text: image.base + text.rva..image.base + text.rva + text.size,
            data: image.base + data.rva..image.base + data.rva + data.size,
            bytes: image.bytes,
            layout: image.layout.clone(),
        });
    }

    // Find the `with` routines at boot rather than in a game frame.
    host::find_with_routines();
    mods::load_all(&paths, &settings);
    match hooks::install() {
        Ok(n) => log::info!("{n} hook site(s) active"),
        Err(e) => log::error!("unable to install hooks ({e})"),
    }
    setup_guard(&image, settings.event_stats);
}

/// The GML error guard and the event observers, if a mod asked for either
/// during init. Anything missing leaves them off (logged); the game runs as
/// without them.
fn setup_guard(image: &win::MainImage, event_stats: bool) {
    use guard::{CALLS, EVENTS};
    let want_observers = guard::observers_registered();
    let mut observers_live = false;
    if guard::requested(EVENTS | CALLS) || want_observers {
        guard::set_image(image.base, image.bytes);
        let t = std::time::Instant::now();
        let img = yyc::Image {
            base: image.base,
            bytes: image.bytes,
        };
        match guard::find_sites(img, &image.layout) {
            Ok(sites) => {
                let rel = |v: &[usize]| {
                    v.iter()
                        .map(|a| format!("{:#x}", a - image.base))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                log::debug!(
                    "GML error guard: ExecuteIt @ {:#x}, YYError @ {:#x}, event globals [{}], executor flags [{}], GC root count @ {:#x}, current event {} (scan {:?})",
                    sites.execute_it - image.base,
                    sites.yyerror - image.base,
                    rel(&sites.event_dwords),
                    rel(&sites.exec_bytes),
                    sites.gc_root_count - image.base,
                    sites.roles.map_or_else(
                        || "not recognised".to_owned(),
                        |r| format!(
                            "kind {:#x} number {:#x} object {:#x}",
                            r.kind - image.base,
                            r.number - image.base,
                            r.object - image.base
                        )
                    ),
                    t.elapsed()
                );
                let events = guard::requested(EVENTS);
                let observers = want_observers && sites.roles.is_some();
                if want_observers && !observers {
                    log::warn!("unable to find the current-event globals; event observers off");
                }
                let using = hooks::DetourUse {
                    events,
                    observers,
                    stats: event_stats,
                };
                match hooks::install_guard(sites, using) {
                    Ok(()) => {
                        guard::set_events_live(events);
                        guard::set_calls_live(true);
                        observers_live = observers;
                        if guard::requested(EVENTS | CALLS) {
                            log::info!(
                                "GML error guard on: events of marked instances {}, mod calls {}",
                                if events { "contained" } else { "not contained" },
                                if guard::requested(CALLS) {
                                    "contained"
                                } else {
                                    "not contained (yet)"
                                }
                            );
                        }
                    }
                    Err(e) => log::warn!("unable to start the GML error guard ({e})"),
                }
            }
            Err(e) => {
                log::warn!("unable to find the GML error guard's runner pieces ({e})");
            }
        }
    }
    let n = guard::freeze_observers(observers_live);
    if n > 0 {
        log::info!("{n} event observer(s) active");
    }
    guard::mark_booted();
}
