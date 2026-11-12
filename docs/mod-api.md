# Writing a mod

A mod is a Rust `cdylib` named `ndmod_<name>.dll`, built against `ndml-sdk`. The loader
calls its init once at game start, before any game code runs. In init the mod reads
its config and registers hooks on GML functions. Everything else happens inside those
hooks.

## 1. Create the crate

```
mods/my-mod/
├─ Cargo.toml
├─ config.toml      default settings (optional)
└─ src/lib.rs
```

`Cargo.toml`:

```toml
[package]
name = "my-mod"
description = "One line shown in the manager"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true

[lib]
name = "ndmod_my_mod"            # the DLL must be named ndmod_*
crate-type = ["cdylib", "rlib"]  # rlib lets `cargo test` run the unit tests

[dependencies]
ndml-sdk.workspace = true
log.workspace = true
serde.workspace = true

[lints]
workspace = true
```

The workspace includes `mods/*` automatically. `cargo build --release` produces
`target/release/ndmod_my_mod.dll`, and the manager's `install` copies it into the game.

## 2. Write it

```rust
#![forbid(unsafe_code)]

use ndml_sdk::{Error, ModContext};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    factor: f64,
}

fn init(ctx: &ModContext) -> Result<(), Error> {
    let cfg: Config = ctx.load_config()?;
    let speed = ctx.variable("magCurrent")?;        // a GML instance variable
    let factor = cfg.factor;

    // Runs around every call of the script; `before` returns state for `after`.
    ctx.hook(
        "gml_Script_powerupAttract",
        move |call| {
            let me = call.instance()?;
            let v = me.get_f64(speed)?;
            me.set_f64(speed, v * factor).then_some(v)
        },
        move |call, saved| {
            if let (Some(me), Some(v)) = (call.instance(), saved) {
                let _ = me.set_f64(speed, v);       // restore the original value
            }
        },
    )?;
    log::info!("active, factor {factor}");
    Ok(())
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));
```

`export_mod!` generates the exports the loader looks for: `ndml_mod_main`,
`ndml_mod_abi_version`, and an `NDML_MOD_INFO` record (name, version, description,
default config). The manager reads that record from the DLL file without loading it.
Without a config, use `ndml_sdk::export_mod!(init);`.

Errors returned from init are logged; the mod's hooks registered so far stay active.
Panics in init or in hooks are caught and logged.

## 3. API

### `ModContext` (init only)

| Method | |
|---|---|
| `load_config::<T>()` | Read `ndml/config/<mod>.toml` into `T` (serde). See *Config* below. |
| `variable(name)` | Resolve a GML instance or global variable by name → `Variable`. `Err` if the game has none by that name. |
| `hook(fn, before, after)` | Wrap a GML function. `before(&mut Call) -> S` runs first; `after(&mut Call, S)` runs after the original. |
| `hook_before(fn, f)` / `hook_after(fn, f)` | One-sided versions. |
| `name()`, `config_dir()`, `default_config()` | Mod name (`ndmod_…`), config folder, shipped default config text. |

Function names are the compiled GML names: `gml_Object_<object>_<Event>_<n>` for object
events (`Create_0`, `Step_0` step, `Step_1` begin step, `Step_2` end step, `Draw_0`,
`Destroy_0`, `Alarm_<k>`, …) and `gml_Script_<name>` for scripts and functions.

Hooks can only be added during init. Callbacks must be `Send + Sync + 'static`. Several
mods may hook the same function; callbacks run in mod load order around the original.

### `Call` (inside a hook)

| Method | |
|---|---|
| `instance()` / `other()` | `self` / `other` of the GML call, if any. |
| `global()` | The global instance (`global.*` variables). |
| `argc()`, `arg(i)` | Script arguments (none for object events). |
| `return_value()` | The script's result (in `after`). |
| `skip_original()` | In `before`: don't run the original function or later hooks. |

### `Instance` (valid only during the callback it came from)

| Method | |
|---|---|
| `get(var)`, `get_f64(var)` | Read an instance variable (as `RValue` / number). |
| `set_f64(var, v)` | Write a number. Returns `false`, without writing, if the current value is a string, array or struct. |
| `builtin(name)`, `builtin_f64(name)` | Read a built-in variable by name: `x`, `y`, `speed`, `hspeed`, `vspeed`, `direction`, `image_angle`, `room_width`, `current_time`, … |
| `set_builtin_f64(name, v)` | Write a built-in through the runtime's setter. `Err(Status::ReadOnly)` for read-only built-ins. |

`Variable` is a cheap `Copy` handle; resolve it once in init and move it into hooks. Its
slot is assigned by the game at startup, so `Variable::slot()` is `None` during init.

### Logging

Use the `log` crate (`log::info!`, `log::debug!`, …). Output goes to
`ndml/logs/ndml.log`, prefixed with the mod name, filtered by `log_level` in
`ndml/ndml.toml`. Keep per-frame logging at `trace`.

## 4. Config

`config.toml` in the crate is the default config, embedded in the DLL. At load:

* if `ndml/config/<mod>.toml` doesn't exist, it is created from the defaults verbatim,
  comments included;
* otherwise its values are laid over the defaults, so keys added in a newer mod version
  get their default values;
* if the file can't be parsed, the defaults are used and a warning is logged.

Write comments in `config.toml`: they are what people see when editing it. The manager
edits values in place and keeps the comments. Arrow keys nudge numbers and toggle
booleans. A string with a fixed set of values becomes a selector when an `options:`
comment line sits directly above it:

```toml
# Where to point.
# options: player, screen_center
anchor = "player"
```

Deserialize such a field into an enum with `#[serde(rename_all = "snake_case")]`.
Validate numbers in the mod (clamp, fall back to defaults) and log a warning rather
than failing.

## 5. Finding something to hook

* Names of objects, events, scripts and variables appear in the executable as plain
  strings (`gml_Object_…`, `gml_Script_…`). The game's localization files (`*.csv` in
  the game folder) help to find the in-game wording of an effect.
* The included **`probe`** mod logs the first calls of chosen functions together with
  selected variables. Enable it in the manager, then set
  `watch = ["gml_Object_oMenu_Step_0"]` and `variables = [...]` in
  `ndml/config/ndmod_probe.toml`.
* Prefer hooking a script used by everything relevant over many object events, and
  prefer changing a value around a call (`before`/`after`, restored afterwards) over
  changing it permanently. Saves and other code then see unchanged values.

## 6. Guidelines

* Mods can be `#![forbid(unsafe_code)]`; the SDK wraps all raw pointers.
* Hooks run on the game thread, once per call, often many times per frame. Don't block
  in them. Do slow work (network, files, IPC) on your own thread and hand data over
  through a channel (see `discord-presence`).
* Keep game-independent logic in plain functions and unit-test it with `cargo test`.

## ABI

Defined in `ndml-sdk/src/abi.rs`. The loader passes `ModInit { host: *const HostApi,
mod_id, name, config_dir }` to `ndml_mod_main`, which returns 0 on success.

| `HostApi` field | |
|---|---|
| `abi_version`, `struct_size` | Version and size of the table as built by the loader. |
| `log(mod_id, level, msg)` | Write to the log. |
| `resolve_variable(name)` | Address of a variable's runtime slot, or null. |
| `hook_gml(mod_id, name, cb, user_data)` | Register a hook callback. |
| `call_next(call)` | Continue the hook chain (next callback or the original). |
| `instance_var(inst, slot, write)` | Pointer to an instance variable's `RValue`. |
| `builtin_get` / `builtin_set` | Built-in variable access by name. |
| `global_instance()` | The global instance, or null. |

A mod built for a different `ABI_VERSION` is refused. Within one version, `HostApi` only
grows at the end. The SDK checks `struct_size` before using newer fields, so mods built
with a newer SDK still load on an older loader and report the missing feature as
`Status::Unsupported`.
