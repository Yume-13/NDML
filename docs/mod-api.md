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
license.workspace = true
repository.workspace = true

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
`target/release/ndmod_my_mod.dll`, and the manager's `install` copies it into the game. The description is shown in the manager's dossier, which has room for about
120 characters.

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
    let pull = ctx.variable("magCurrent")?;         // a GML instance variable
    let factor = cfg.factor;

    // Runs around every call of the script; `before` returns state for `after`.
    ctx.hook(
        "gml_Script_powerupAttract",
        move |call| {
            let me = call.instance()?;
            let v = me.get_f64(pull)?;
            me.set_f64(pull, v * factor).then_some(v)
        },
        move |call, saved| {
            if let (Some(me), Some(v)) = (call.instance(), saved) {
                let _ = me.set_f64(pull, v);        // restore the original value
            }
        },
    )?;
    log::info!("ready, factor {factor}");
    Ok(())
}

ndml_sdk::export_mod!(init, defaults = include_str!("../config.toml"));
```

with `config.toml`:

```toml
# How much stronger the pull is.
factor = 2.0
```

`export_mod!` generates the exports the loader looks for: `ndml_mod_main`,
`ndml_mod_abi_version`, and an `NDML_MOD_INFO` record (name, version, description,
default config). The manager reads that record from the DLL file without loading it.
Without a config, use `ndml_sdk::export_mod!(init);`. The macro also defines
`NDML_DEFAULT_CONFIG`, the shipped config text, for unit tests.

If init returns an error or panics, the error is logged and the mod is not loaded: the
hooks and observers it registered so far are dropped. Panics in hooks are caught and
logged; the game continues.

## 3. API

### `ModContext` (init only)

| Method | |
|---|---|
| `load_config::<T>()` | Read `ndml/config/<mod>.toml` into `T` (serde). See *Config* below. |
| `variable(name)` | Resolve a GML instance or global variable by name → `Variable`. `Err` if the game has none by that name. |
| `script(name)` | Resolve a GML script (`gml_Script_x` or `x`) → `Script`, for calling it. |
| `builtin(name)` | Name a GameMaker built-in function (`instance_exists`, `draw_text`, …) → `Builtin`. The name is checked on the first call. |
| `hook(fn, before, after)` | Wrap a GML function. `before(&mut Call) -> S` runs first; `after(&mut Call, S)` runs after the original. |
| `hook_before(fn, f)` / `hook_after(fn, f)` | One-sided versions. |
| `hook_builtin(name, before, after)` | Wrap a built-in function; every call of it, from the game or from mods, runs the hook. |
| `observe_event(kind, number, f)` | Run `f` before and after every compiled event of that kind and number (`-1` = any), whatever object runs it. User event `n`: kind `EVENT_OTHER`, number `user_event(n)`. |
| `guard(events, calls)` | Ask for the GML error guard (see below). |
| `name()`, `config_dir()`, `default_config()` | Mod name (`ndmod_…`), config folder, shipped default config text. |

Function names are the compiled GML names: `gml_Object_<object>_<Event>_<n>` for object
events (`Create_0`, `Step_0` step, `Step_1` begin step, `Step_2` end step, `Draw_0`,
`Draw_64` GUI layer, `Destroy_0`, `Alarm_<k>`, …) and `gml_Script_<name>` for scripts
and functions.

Hooks can only be added during init, at most 512 hooked functions for all mods together.
Callbacks must be `Send + Sync + 'static`. Several mods may hook the same function;
callbacks run in mod load order around the original.

A function whose first bytes the loader's detour cannot move (one that starts with a
call, for example) can't be hooked: the hook methods return
`Err(Error::Hook { status: Status::Unsupported, .. })` and the loader logs the reason. A
mod that passes the error on with `?` fails its init. Rarely, a hook accepted at init
cannot be installed when the loader enables the hooks (no code memory within reach of
the function, for example); the loader then logs `hook <name> refused: ...` and that function runs
unhooked.

### `Call` (inside a hook)

| Method | |
|---|---|
| `instance()` / `other()` | `self` / `other` of the GML call, if any. |
| `global()` | The global instance (`global.*` variables). |
| `argc()`, `arg(i)`, `arg_value(i)` | Arguments (none for object events), as `RValue` or as an owned `Value`. |
| `return_value()`, `result()` | The result (in `after`), as `RValue` or `Value`. |
| `set_return(v)` | Replace a script's or built-in's result. |
| `skip_original()` | In `before`: don't run the original function or later mods' hooks. |
| `call_script(script, args)` | Run a GML script with this call's `self`/`other`; returns its result as a `Value`. |
| `call_builtin(builtin, args)` | Run a built-in function the same way. Drawing built-ins show only inside Draw events. |
| `call_builtin_held(builtin, args)` | As `call_builtin`, but keeps an array, struct or method result as a `Held`, which can be passed back to built-ins as `Arg::Held` within this hook call. |
| `with(target, f)` | GML `with (target)`: run `f` for every instance an instance id or `Value::object(index)` selects, with that instance as `self`. |
| `builtin_available(builtin)` | Whether the game has that built-in, without calling it. |
| `key_down(vk)` | Whether a key is held, while the game window is in front (`input::vk_code("F5")` gives the code). |

Calls into the game run synchronously on the game thread, and run the hooks of the
function called (yours included).

### `Instance` (valid only during the callback it came from)

| Method | |
|---|---|
| `get(var)`, `get_f64(var)`, `get_value(var)` | Read an instance variable (as `RValue`, number or `Value`). |
| `set_f64(var, v)` | Write a number. Returns `false`, without writing, if the current value is a string, array or struct. |
| `set(var, value)` | Write any `Value`, strings included; the old value is released. |
| `builtin(name)`, `builtin_f64(name)`, `builtin_value(name)` | Read a built-in variable by name: `x`, `y`, `speed`, `hspeed`, `vspeed`, `direction`, `image_angle`, `room_width`, `current_time`, … |
| `set_builtin_f64(name, v)` | Write a built-in through the runtime's setter. `Err(Status::ReadOnly)` for read-only built-ins. |
| `call_script(script, args)`, `call_builtin(builtin, args)` | Run game code with this instance as `self`. |
| `with(target, f)` | GML `with (target)` with this instance as `other`, as `Call::with` does: how code that holds only an instance (an event observer) reaches another one by id. |
| `key()` | An identity for per-instance state, stable while the instance exists. |

`Variable`, `Script` and `Builtin` are cheap `Copy` handles; resolve them once in init
and move them into hooks. A variable's slot is assigned by the game at startup, so
`Variable::slot()` is `None` during init.

### `Value`

An owned copy of a GML value: `Undefined`, `Real(f64)`, `Int(i64)`, `Bool`, `Str(String)`,
`Ref(u64)` (object, instance and other references), `Ptr(u64)`, or `Other { kind }` for
arrays, structs and methods. `as_f64()`, `as_i64()`, `as_str()` and `as_bool()` read it;
numbers, `bool`, `&str` and `String` convert into it, so `call_script(s, &[2.0.into()])`
works. `instance_number(&v)` gives the id of an instance reference.

### Example: drawing and calling a built-in

```rust
#![forbid(unsafe_code)]

use ndml_sdk::{Error, ModContext, Value};

fn init(ctx: &ModContext) -> Result<(), Error> {
    let draw_text = ctx.builtin("draw_text")?;
    ctx.hook_after("gml_Object_oMenu_Draw_64", move |call| {
        let args = [Value::Real(16.0), Value::Real(16.0), "modded".into()];
        if let Err(e) = call.call_builtin(draw_text, &args) {
            log::warn!("{e}");
        }
    })
}

ndml_sdk::export_mod!(init);
```

### GML errors

A GML runtime error inside a mod's call ends the game process, as it would in the game
itself. Check what can be checked first (`instance_exists` before touching an id). A
mod that must call code that may fail asks for the guard in init with
`ctx.guard(events, calls)`:

* `calls`: an error inside `call_script`/`call_builtin` returns
  `Err(Error::Call { status: Status::GmlError, .. })` instead.
* `events`: events of instances named with `Call::guard_mark(&ids)` (optionally widened
  with `guard_scope`) are abandoned on an error instead of ending the game; the game
  continues. `Call::guard_take()` returns the errors caught since the last call, and
  `Call::guard_expect()` marks errors inside one call as expected.

### Logging

Use the `log` crate (`log::info!`, `log::debug!`, …). Output goes to
`ndml/logs/ndml.log`, prefixed with the mod name, filtered by `log_level` in
`ndml/ndml.toml`; the last nine launches' logs are kept beside it. Keep per-frame
logging at `trace`.

### Timing (`ndml_sdk::profile`)

Off unless the mod turns it on; while off, each hook call costs one atomic load. The
state is per mod DLL, and the rustdoc is the reference.

* `profile::enable(true)` times every hook and observer the mod registered (before and
  after apart, with call counts). Call `profile::frame(Instant::now())` once a frame
  from the same place: it returns the frame that just ended (`Frame::line(top)` gives
  one line: wall time, the mod's time, the remainder, spans and the largest hooks).
* Sections of the mod's own code: `static WORK: Span = Span::new("work");`, then
  `profile::span(&WORK, || ...)`, or `let _t = profile::enter(&WORK);` (an `Entered`
  guard, timed until it drops) for code that is not one closure.
* `profile::time_sides(true)` keeps only two sums, the mod's time in step-side and in
  draw-side hooks (draw events, observers of draw events, `draw_` builtins);
  `profile::take_sides()` returns them and resets them.

## 4. Config

`config.toml` in the crate is the default config, embedded in the DLL. It defines the
settings, their order, their comments and their defaults; the player's file supplies
the values. At load (and on each `install` by the manager):

* if `ndml/config/<mod>.toml` doesn't exist, it is created from the defaults verbatim,
  comments included;
* if its settings or comments differ from the shipped ones, it is rewritten in the
  shipped layout with the player's values kept, new settings added at their default,
  and settings the mod no longer has dropped; the previous file is kept as
  `<mod>.toml.bak`;
* a value of the wrong type falls back to that setting's default; a file that can't be
  parsed is left as it is, the defaults are used and an error is logged.

Because unknown keys are dropped, the shipped file must list every key `T` reads. A
unit test checks it:

```rust
#[test]
fn shipped_config_lists_every_key() {
    ndml_sdk::config::check_shipped_keys::<Config>(NDML_DEFAULT_CONFIG, &[]).unwrap();
}
```

Write comments in `config.toml`: they are what people see. The comment directly above a
key is its help in the manager; the paragraphs above the first key's own comment are
the mod's introduction. Arrow keys nudge numbers and toggle booleans; Enter types a
value. A string with a fixed set of values becomes a selector when an `options:`
comment sits above it, and a list of strings with `options:` becomes a checklist:

```toml
# Where to point.
# options: player, screen_center
anchor = "player"

# Label colour. Any colour typed as #rrggbb is accepted too.
# options: auto, gold #e8b84a = Gold, red #e05050 = Red, #rrggbb
colour = "auto"

# Which corners show the timer.
# options:
#   Top:
#     top_left = Top left
#     top_right = Top right (default)
#   Bottom:
#     bottom_left = Bottom left
corners = ["top_right"]
```

Entries are `value [#rrggbb] [= Label (hint)]`; in the block form a line ending in `:`
is a group heading. Deserialize a selector into an enum with
`#[serde(rename_all = "snake_case")]`. Validate numbers in the mod (clamp, fall back to
defaults) and log a warning rather than failing.

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
* Hooks are re-entered: a call into the game from a hook runs the hooks of what it
  calls, on the same thread. Never hold one of your own locks across `call_script`,
  `call_builtin`, `with` or a hooked built-in: take what you need, drop the guard, then
  call. A lock taken twice on the game thread hangs the game.
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
| `key_down(vk)` | Keyboard state. |
| `script_find`, `script_call` | Find and call a compiled script. |
| `builtin_fn_find`, `builtin_fn_call` | Find and call a built-in function. |
| `value_set_string`, `value_free`, `value_get_string` | Create, release and read game values. |
| `with_instances` | GML `with`. |
| `guard_request`, `guard_mark`, `guard_take`, `guard_scope`, `guard_expect` | The GML error guard. |
| `observe_event` | Event observers. |

A mod built for a different `ABI_VERSION` is refused. Within one version, `HostApi` only
grows at the end. The SDK checks `struct_size` before using newer fields, so mods built
with a newer SDK still load on an older loader and report the missing feature as
`Status::Unsupported`.
