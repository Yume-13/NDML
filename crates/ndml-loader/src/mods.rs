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

//! Discovering and initialising mod DLLs from `ndml/mods/`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use ndml_sdk::abi::{ABI_VERSION, ModInit, StrRef};

use ndml_common::{MOD_PREFIX, Paths, Settings};

use crate::win::ModLibrary;

/// Mod DLLs in load order (alphabetical by file name), minus disabled ones.
fn discover(paths: &Paths, settings: &Settings) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(&paths.mods) else {
        return Vec::new();
    };
    let mut dlls: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll")))
        .filter(|p| stem(p).starts_with(MOD_PREFIX))
        .collect();
    dlls.sort();
    dlls.retain(|p| {
        let stem = stem(p);
        let disabled = settings
            .disabled_mods
            .iter()
            .any(|d| d.eq_ignore_ascii_case(stem));
        if disabled {
            log::info!("{stem} skipped: disabled");
        }
        !disabled
    });
    dlls
}

fn stem(p: &Path) -> &str {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("?")
}

/// Load and initialise every mod. Mods register hooks during init.
pub fn load_all(paths: &Paths, settings: &Settings) {
    let dlls = discover(paths, settings);
    // Leaked: ModInit strings and log targets must outlive the mods.
    let names: Vec<&'static str> = dlls.iter().map(|p| &*stem(p).to_owned().leak()).collect();
    crate::host::set_mod_names(names.clone());
    let config_dir: &'static str = paths.config.to_string_lossy().into_owned().leak();

    for (id, (path, name)) in dlls.iter().zip(names).enumerate() {
        match load_one(path, id as u32, name, config_dir) {
            Ok(()) => log::info!("{name} loaded"),
            Err(e) => {
                log::error!("unable to load {name} ({e})");
                drop_registrations(id as u32, name);
            }
        }
    }
}

/// A mod that did not load keeps nothing it registered before failing: its
/// hooks and event observers go, so no part of it runs. Its DLL stays mapped
/// (as every mod's does): a thread it started, a callback's `user` pointer
/// or a log target may still point into it, and unmapping would leave those
/// dangling. A failure here is logged; the game goes on.
fn drop_registrations(mod_id: u32, name: &str) {
    let dropped = std::panic::catch_unwind(|| {
        (
            crate::hooks::drop_mod(mod_id),
            crate::guard::drop_observers(mod_id),
        )
    });
    match dropped {
        Ok((0, 0)) => {}
        Ok((hooks, observers)) => log::warn!(
            "{name}: dropped the {hooks} hook(s) and {observers} event observer(s) it registered before failing"
        ),
        Err(_) => log::error!("{name}: could not drop what it registered before failing"),
    }
}

fn load_one(
    path: &Path,
    mod_id: u32,
    name: &'static str,
    config_dir: &'static str,
) -> Result<(), String> {
    let lib = ModLibrary::load(path)?;
    let mod_abi = lib.abi_version();
    if mod_abi != ABI_VERSION {
        return Err(format!(
            "built for ABI v{mod_abi}, loader is v{ABI_VERSION}; rebuild the mod"
        ));
    }
    let init = ModInit {
        host: &raw const crate::host::HOST_API,
        mod_id,
        name: StrRef::new(name),
        config_dir: StrRef::new(config_dir),
    };
    match lib.init(&init) {
        0 => Ok(()),
        code => Err(format!("init returned {code}")),
    }
}
