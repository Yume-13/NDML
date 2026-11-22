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

//! # Probe (developer tool)
//!
//! Logs the first few calls of chosen GML functions together with selected
//! instance variables and script arguments, showing whether a hook target
//! fires, when, on which instance, and with what values. Inert by default
//! (empty `watch`).
//!
//! `ndml/config/ndmod_probe.toml`:
//! ```toml
//! watch = ["gml_Object_oMenu_Step_0", "gml_Script_powerupAttract"]
//! variables = ["magDist", "magCurrent"]
//! max_calls = 3
//! ```

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ndml_sdk::abi::kind;
use ndml_sdk::{Call, Error, ModContext, RValue, Variable};
use serde::Deserialize;

/// `ndml/config/ndmod_probe.toml`; shipped defaults in `config.toml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// GML functions to watch (`gml_Object_*` events or `gml_Script_*`).
    pub watch: Vec<String>,
    /// Instance variables (variable-map slots) printed for each logged call.
    pub variables: Vec<String>,
    /// Calls logged per function before going quiet.
    pub max_calls: u32,
}

/// Human-readable rendering of an `RValue`.
#[must_use]
pub fn describe(v: &RValue) -> String {
    match v.type_tag() {
        kind::UNDEFINED => "undefined".into(),
        kind::UNSET => "unset".into(),
        kind::STRING => "<string>".into(),
        kind::ARRAY => "<array>".into(),
        kind::OBJECT => "<struct>".into(),
        kind::PTR => format!("<ptr {:#x}>", v.payload),
        kind::REF => format!("<ref {:#x}>", v.payload),
        _ => v
            .as_f64()
            .map_or_else(|| format!("<kind {}>", v.type_tag()), |n| format!("{n}")),
    }
}

fn line(name: &str, n: u32, call: &Call<'_>, vars: &[(String, Variable)]) -> String {
    let mut s = format!("{name} #{n}");
    if let Some(me) = call.instance() {
        for (vname, var) in vars {
            let slot = var
                .slot()
                .map_or_else(|| "unassigned".to_owned(), |s| s.to_string());
            let value = me
                .get(*var)
                .map_or_else(|| "n/a (built-in)".to_owned(), |v| describe(&v));
            let _ = write!(s, " {vname}[{slot}]={value}");
        }
    } else {
        s.push_str(" (no self)");
    }
    for i in 0..call.argc() {
        if let Some(a) = call.arg(i) {
            let _ = write!(s, " arg{i}={}", describe(&a));
        }
    }
    s
}

fn init(ctx: &ModContext) -> Result<(), Error> {
    let config: Config = ctx.load_config()?;
    if config.watch.is_empty() {
        log::debug!("no functions to watch; idle");
        return Ok(());
    }
    let mut vars = Vec::new();
    for name in &config.variables {
        match ctx.variable(name) {
            Ok(v) => vars.push((name.clone(), v)),
            Err(e) => log::warn!("{e}"),
        }
    }
    let vars = Arc::new(vars);
    for name in config.watch.clone() {
        let calls = Arc::new(AtomicU32::new(0));
        let max = config.max_calls;
        let (vars, label) = (Arc::clone(&vars), name.clone());
        let result = ctx.hook(
            &name,
            move |call| {
                let n = calls.fetch_add(1, Ordering::Relaxed);
                let logged = n < max;
                if logged {
                    log::info!("{}", line(&label, n + 1, call, &vars));
                }
                logged
            },
            |call, logged| {
                if logged && let Some(ret) = call.return_value() {
                    log::info!("  -> returned {}", describe(&ret));
                }
            },
        );
        match result {
            Ok(()) => log::info!("watching {name}"),
            Err(e) => log::warn!("{e}"),
        }
    }
    Ok(())
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_parse() {
        let cfg: Config = ndml_sdk::config::parse_defaults(NDML_DEFAULT_CONFIG).unwrap();
        assert!(cfg.watch.is_empty(), "probe must ship inert");
    }

    #[test]
    fn describes_values() {
        assert_eq!(describe(&RValue::real(300.0)), "300");
        let u = RValue {
            payload: 0,
            flags: 0,
            kind: kind::UNDEFINED,
        };
        assert_eq!(describe(&u), "undefined");
    }
}
