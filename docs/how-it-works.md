# How NDML works

Nova Drift is a GameMaker game built with **YYC**, which compiles the game's GML scripts
and object events to native x64 code inside `NovaDrift.exe`. There is no bytecode to
patch. Instead, NDML finds the compiled functions by name at startup and puts inline
hooks on the ones mods ask for.

## Components

| Crate | Output | Role |
|---|---|---|
| `ndml-loader` | `version.dll` | Proxy DLL in the game folder. Boots NDML, loads mods, owns all hooks. |
| `ndml-sdk` | (library) | ABI shared with the loader (`abi.rs`) and the safe API mods use. |
| `ndml-common` | (library) | Folder layout and `ndml.toml` settings, shared by loader and manager. |
| `ndml-cli` | `ndml.exe` | Manager: install, configure, read logs, launch. Never touches the game process. |

Only `ndml-sdk::abi` crosses a DLL boundary (loader ↔ mod). It is plain `#[repr(C)]` data
and function pointers.

## Boot sequence

1. **Windows loads `version.dll`.** The game imports `VERSION.dll`; a copy in the game
   folder takes precedence over the system one. NDML forwards all of the DLL's real
   exports to the system `version.dll`, so the game sees no difference. Loaded by any
   other program, it is only that proxy.
2. **`DllMain`** runs under the Windows loader lock, where loading libraries or doing
   file I/O is unsafe. It only installs a detour on the executable's entry point.
3. **Entry-point detour.** Runs once, before any game code:
   1. read `ndml/ndml.toml` and start logging to `ndml/logs/ndml.log`; a key that can't
      be read is logged and left at its default;
   2. scan the executable for the GameMaker runtime tables (below);
   3. load every `ndml/mods/ndmod_*.dll` that isn't disabled, in alphabetical order,
      check its ABI version and call its init, where it registers hooks;
   4. enable all hooks;
   5. if a mod asked for them, start the GML error guard and the event observers;
   6. continue to the game's real entry point.

If any step fails, the failure is logged and the game starts without that feature, or
unmodded. NDML never stops the game from launching. A failure before the log is opened
(the entry-point detour, or creating the `ndml/` folder) is reported only to the debugger
output (`OutputDebugString`); `ndml.log` is then still the previous session's (its first
line gives the start time). A mod whose init fails keeps none of the hooks or observers
it registered; the other mods load as usual.

## Finding functions and variables

The executable keeps two name-keyed tables in its `.data` section, and NDML locates both
by their shape rather than by fixed addresses:

* **Function table**: 24-byte entries `{ const char* name; void* code; YYVAR* var; }`
  with names like `gml_Object_oPlayer_Step_0` (an object event) or
  `gml_Script_powerupAttract` (a script or function).
* **Variable descriptors** (`YYVAR`): `{ const char* name; int32 slot; }`. The slot is
  `-1` in the file and assigned by the runtime during startup, and compiled code reads
  it on every access. NDML records the *address* of each slot and reads it when a mod
  uses the variable.

Function signatures:

```c
void    gml_Object_<obj>_<Event>_<n>(CInstance* self, CInstance* other);
RValue* gml_Script_<name>(CInstance* self, CInstance* other,
                          RValue* result, int argc, RValue** argv);
void    builtin_routine(RValue* result, CInstance* self, CInstance* other,
                        int argc, RValue* args);
```

**Built-in functions** (`instance_exists`, `draw_text`, …) live in a separate runtime
table that the runtime fills during startup. NDML finds it by shape on first use, so a
built-in name is checked the first time a mod calls it. For hooking, the routines are
found at boot from the runtime's own registration code, before that table exists.
Functions of the game's extension DLLs (Steamworks) are found by their exported names
and called the same way.

## Hooks

Each hooked function gets one inline detour to a small thunk. Thunks are generated at
compile time with the slot index baked in, so no code is generated at runtime. There are
512 slots, shared by events, scripts and built-in functions of all mods; several mods
hooking the same function share one slot. The thunk builds a `HookCall` (`self`,
`other`, arguments, return value) and walks the chain of mod callbacks for that function,
in mod load order, the first registered outermost. The last link calls the original
function through the detour's trampoline, at most once. A callback can run code before
and after the original, read and change arguments and variables, replace the return
value, or skip the original together with the rest of the chain.

A detour overwrites the function's first 5 bytes with a `jmp` to a small relay placed
within reach of the function, and copies the whole instructions in those bytes to a
trampoline beside the relay, followed by a jump back.
Copied instructions that depend on their address (`[rip+disp32]` operands, relative
jumps and conditional branches) are rewritten to reach the same targets. Anything the
detour cannot move safely (a call, a loop instruction, a branch into the copied bytes,
an instruction it does not know) is refused: registering the hook fails with
`Status::Unsupported`, the log says why (`hook <name> refused: Unsupported (...)`), and
the function runs unhooked.

Hookable: object events `gml_Object_*`, scripts `gml_Script_*`, and built-in functions
named `builtin:<name>`. The `gml_GlobalScript_*` wrappers are not hookable.

Hooks are registered during mod init only. The loader then freezes the registry and
enables the detours, so dispatch takes no locks. A hook may run inside another hook on
the same thread (a mod's call into the game runs that function's hooks too); a mod must
not hold one of its own locks across a call into the game.

Every hook site counts its calls. At `log_level = "debug"` the loader logs a summary
once a minute (`hook calls in last 60s: …`), which shows whether hooks are firing.

## Calling into the game

From inside a hook a mod can call compiled scripts (by address from the function table,
checked on every call), built-in functions, and GML `with (target)` over the instances a
target selects. Arguments and results cross as values the SDK owns: strings are created
with the runtime's own string routine and every owning value is released with the
runtime's release routine, both found by code shape (the string routine is checked with
a test string before it is used). Calls run
synchronously on the game thread. Whatever the loader could not find is logged once,
and only the calls that need it return `NotReady`.

## Game values

GameMaker values are `RValue`s, 16 bytes: an 8-byte payload, 4-byte flags, and a 4-byte
type tag, of which the low 24 bits count. Common tags: 0 real (f64), 1 string, 2 array,
5 undefined, 6 struct/object, 7 int32, 10 int64, 13 bool, 15 reference, `0xFFFFFF`
unset. Strings, arrays and structs are reference-counted, so NDML never overwrites them
by writing raw fields; a value that owns memory is replaced through the runtime, which
releases the old one.

* **Instance variables** (slots ≥ 100000) live in each instance's variable map. NDML
  reads and writes them through the same accessors compiled code uses: virtual
  functions 1 (read) and 2 (write, creates if missing) of the instance object.
* **Built-in variables** (`x`, `y`, `speed`, `hspeed`, `direction`, `room_width`, …)
  have their own getter and setter functions in a runtime table of 32-byte entries
  `{ char* name; getter; setter (null if read-only); bool can_set; }`. The runtime fills
  it during startup. NDML finds it by shape the first time a mod uses a built-in, and
  checks that it contains `x`, `y`, `speed` and `room_width`.
* **Global variables** (`global.*`) are instance variables of one global object. Compiled
  code reaches it through a pointer in `.data`, loaded with
  `mov rcx, [rip+G]; mov r8, [rcx]` at thousands of sites. NDML takes the most common
  such `G` and reads `*G` when a mod asks for the global instance.

## GML error guard and event observers

A GML runtime error is a C++ exception thrown by the runtime; the game's own handler
shows the "Code Error" dialog and ends the process. Both features below are off unless a
mod asks for them during init, and both run on one detour on the runtime's `ExecuteIt`
(which runs one compiled event for one instance), found by shape at boot.

* **Event guard**: events of instances a mod marked (and, if the mod widens the scope,
  what they create, or every instance of named objects) run inside an exception frame.
  An error there abandons that event only, like a GML `try`/`catch` around its body:
  the runtime's bookkeeping and the drawing state (surface target, shader, blend mode,
  alpha, colour) are put back, the error is reported to the mod, and the game continues.
  Errors in the game's own instances still reach the game.
* **Call guard**: an error inside a script or built-in a mod calls returns
  `Status::GmlError` to the mod instead of ending the process.
* **Event observers**: a mod's callback runs before and after every compiled event of a
  given kind and number (for example user event 0 of every object), without one hook per
  object. At most 64 observers.

`event_stats = true` in `ndml.toml` logs the detour's cost per frame every 10 seconds.

## Logging

The loader and every mod log to `ndml/logs/ndml.log`, one line per message:
`[seconds] LEVEL target: message`, where `target` is the loader module (`ndml_loader`,
`ndml_loader::mods`, …) or the mod's DLL name. `log_level` filters it; `console = true`
also opens a console window showing the same lines. The manager's LOG tab shows the end
of this file.

The first line gives the session's start in local time and UTC. At the next launch,
before the new log is opened, the previous one is renamed after that start,
`ndml-YYYY-MM-DD_HH-MM-SS.log` in local time (with a `Z` and in UTC when the local time
can't be read), and the oldest renamed logs are deleted so that nine remain beside the
running one. Nothing else in the folder is renamed or deleted; a file that can't be is
skipped and mentioned in the new log.

## Robustness and safety

* Mod callbacks run inside `catch_unwind`. A panicking mod is logged and the game
  continues; the original function still runs.
* The original game function is always called outside `catch_unwind`, through
  `extern "C-unwind"` functions, so the runtime's own exceptions propagate normally.
* `unsafe` code is limited to the FFI boundary: the loader's `entry`, `win`, `hooks`,
  `host` and `proxy` modules and the SDK's `ffi` module. Every other module, the
  manager, `ndml-common` and all included mods are `#![forbid(unsafe_code)]`.
* No address in the game is hard-coded: everything is found by name or by code shape,
  and anything not found turns off only the feature that needs it.
* NDML makes no network connections and writes only below the game folder. Mods are
  native code and are not sandboxed: a mod can do anything the game process can.

## Limitations

* Hooks can only be added during mod init, at most 512 hooked functions in all.
* A function whose first bytes the detour cannot move can't be hooked (see *Hooks*). In
  Nova Drift 1.2.38.4 this applies to 20 built-in functions that start with a call,
  among them `room_goto`, `draw_set_font` and `part_system_update`.
* A branch later in a hooked function back into its first 5 bytes is not detected: a
  loop back to the start would run the hook again on each pass, one back into the
  patched bytes would run them as code.
* Arrays, structs and methods can't be built by a mod; they can be held and passed back
  to built-in functions (`array_get`, `variable_struct_get`, …) within one hook call.
* Asynchronous GameMaker events (such as Steam callbacks) can't be received by mods.
* Without the call guard, a GML runtime error inside a mod's call ends the game process.
* Only compiled events report their kind and number to observers; room and instance
  creation code does not.
