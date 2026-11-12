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

//! # ndml-sdk
//!
//! Everything a Nova Drift mod needs: the loader ABI ([`abi`]) and a safe layer
//! over it. A mod is a `cdylib` with `#![forbid(unsafe_code)]` that calls
//! [`export_mod!`] with an init function and its shipped default config:
//!
//! ```ignore
//! #![forbid(unsafe_code)]
//! #[derive(serde::Deserialize)]
//! struct Config { factor: f64 }
//!
//! fn init(ctx: &ndml_sdk::ModContext) -> Result<(), ndml_sdk::Error> {
//!     let cfg: Config = ctx.load_config()?;            // ndml/config/<mod>.toml
//!     let speed = ctx.variable("speed")?;
//!     ctx.hook_before("gml_Object_oPlayer_Step_0", move |call| {
//!         if let Some(me) = call.instance() {
//!             log::trace!("speed = {:?}", me.get_f64(speed));
//!         }
//!     })
//! }
//! ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));
//! ```
//!
//! Unsafe code is confined to the private `ffi` module; everything else here
//! (and in every mod) is safe Rust. Mods log through the `log` crate; output
//! lands in `ndml/logs/ndml.log`. See `docs/mod-api.md`.

#![deny(unsafe_code)] // only `ffi` opts out, with a module-level allow

pub mod abi;
pub mod config;
mod ffi;
mod logger;
pub mod modinfo;

use std::fmt;
use std::path::{Path, PathBuf};

pub use abi::{RValue, Status};
#[doc(hidden)]
pub use ffi::__run_mod_main;
pub use ffi::{Call, Instance, Variable};
use serde::de::DeserializeOwned;

/// Errors surfaced to mod authors.
#[derive(Debug)]
pub enum Error {
    /// No GML variable with this name exists in the game build.
    VariableNotFound(String),
    /// The loader refused a hook.
    Hook { function: String, status: Status },
    /// Anything else, with a human-readable reason.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VariableNotFound(n) => {
                write!(f, "GML variable `{n}` not found in this game build")
            }
            Self::Hook { function, status } => write!(f, "cannot hook `{function}`: {status:?}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

/// What a mod's init function receives.
#[derive(Debug)]
pub struct ModContext {
    host: ffi::Host,
    name: String,
    config_dir: PathBuf,
    defaults: &'static str,
}

impl ModContext {
    pub(crate) fn new(
        host: ffi::Host,
        name: String,
        config_dir: PathBuf,
        defaults: &'static str,
    ) -> Self {
        Self {
            host,
            name,
            config_dir,
            defaults,
        }
    }

    /// The mod's name (its DLL file stem).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Directory holding this mod's config files.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The shipped default config text given to [`export_mod!`].
    #[must_use]
    pub fn default_config(&self) -> &'static str {
        self.defaults
    }

    /// Load `<config_dir>/<mod name>.toml` over the shipped defaults (written
    /// verbatim if the file is missing). A broken config file is logged and the
    /// defaults are used; only broken *shipped* defaults are an error.
    pub fn load_config<T: DeserializeOwned>(&self) -> Result<T, Error> {
        let path = self.config_dir.join(format!("{}.toml", self.name));
        config::load(&path, self.defaults).map_err(Error::Other)
    }

    /// Look up a GML instance variable by name.
    pub fn variable(&self, name: &str) -> Result<Variable, Error> {
        self.host
            .resolve_variable(name)
            .ok_or_else(|| Error::VariableNotFound(name.to_owned()))
    }

    /// Run `f` before the GML function `function` (which still runs afterwards
    /// unless `f` calls [`Call::skip_original`]).
    pub fn hook_before<F>(&self, function: &str, f: F) -> Result<(), Error>
    where
        F: Fn(&mut Call<'_>) + Send + Sync + 'static,
    {
        self.hook(function, f, |_, ()| {})
    }

    /// Run `f` after the GML function `function` has run.
    pub fn hook_after<F>(&self, function: &str, f: F) -> Result<(), Error>
    where
        F: Fn(&mut Call<'_>) + Send + Sync + 'static,
    {
        self.hook(function, |_| {}, move |call, ()| f(call))
    }

    /// Wrap the GML function `function`: `before` runs first and returns some
    /// state, then the original (and any other mods' hooks) run, then `after`
    /// receives that state. Use this to temporarily change and then restore
    /// game state.
    ///
    /// Supported targets: object events (`gml_Object_<obj>_<Event>_<n>`) and
    /// scripts (`gml_Script_<name>`). Hooks can only be added during init.
    /// Panics inside `before`/`after` are caught and logged; the game continues.
    pub fn hook<S, B, A>(&self, function: &str, before: B, after: A) -> Result<(), Error>
    where
        S: 'static,
        B: Fn(&mut Call<'_>) -> S + Send + Sync + 'static,
        A: Fn(&mut Call<'_>, S) + Send + Sync + 'static,
    {
        match self.host.hook(function, before, after) {
            Status::Ok => Ok(()),
            status => Err(Error::Hook {
                function: function.to_owned(),
                status,
            }),
        }
    }
}

/// Export a mod's entry points and embedded metadata.
///
/// ```ignore
/// ndml_sdk::export_mod!(init);                                            // no config
/// ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml")); // with config
/// ```
///
/// Also defines `pub const NDML_DEFAULT_CONFIG: &str` (use it in a unit test with
/// [`config::parse_defaults`]) and the exported `NDML_MOD_INFO` record read by
/// tools (see [`modinfo`]). The generated exports are the only unsafe-attributed
/// items a mod contains, and they come from this macro, so mods themselves can
/// `#![forbid(unsafe_code)]`.
#[macro_export]
macro_rules! export_mod {
    ($init:path) => {
        $crate::export_mod!($init, defaults = "");
    };
    ($init:path, defaults = $($defaults:tt)+) => {
        /// The shipped default config (TOML) for this mod.
        pub const NDML_DEFAULT_CONFIG: &str = $($defaults)+;

        /// Metadata record read by tools; format in `ndml_sdk::modinfo`.
        #[unsafe(no_mangle)]
        pub static NDML_MOD_INFO: &str = concat!(
            "\u{1}NDML-MODINFO\u{2}",
            env!("CARGO_PKG_NAME"), "\u{1f}",
            env!("CARGO_PKG_VERSION"), "\u{1f}",
            env!("CARGO_PKG_DESCRIPTION"), "\u{1f}",
            $($defaults)+,
            "\u{3}"
        );

        #[unsafe(no_mangle)]
        pub extern "C" fn ndml_mod_abi_version() -> u32 {
            $crate::abi::ABI_VERSION
        }

        /// # Safety
        /// Called by the NDML loader only.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn ndml_mod_main(init: *const $crate::abi::ModInit) -> i32 {
            // SAFETY: the loader passes a valid ModInit.
            unsafe { $crate::__run_mod_main(init, $init, NDML_DEFAULT_CONFIG) }
        }
    };
}
