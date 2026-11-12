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
   exports to the system `version.dll`, so the game sees no difference.
2. **`DllMain`** runs under the Windows loader lock, where loading libraries or doing
   file I/O is unsafe. It only installs a detour on the executable's entry point.
3. **Entry-point detour.** Runs once, before any game code:
   1. read `ndml/ndml.toml`, start logging to `ndml/logs/ndml.log`;
   2. scan the executable for the GameMaker runtime tables (below);
   3. load every `ndml/mods/ndmod_*.dll` that isn't disabled, check its ABI version and
      call its init, where it registers hooks;
   4. enable all hooks;
   5. continue to the game's real entry point.

If any step fails, the failure is logged and the game starts without that feature, or
unmodded. NDML never stops the game from launching.

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
```

## Hooks

Each hooked function gets one inline detour to a small thunk. Thunks are generated at
compile time for up to 128 functions of each kind, with the slot index baked in, so no
code is generated at runtime. The thunk builds a `HookCall` (`self`, `other`, arguments,
return value) and walks the chain of mod callbacks for that function, in mod load order.
The last link calls the original function through the detour's trampoline. A callback
can run code before and after the original, read and change arguments and instance
variables, or skip the original.

Hooks are registered during mod init only. Dispatch afterwards takes no locks.

Every hook site counts its calls. At `log_level = "debug"` the loader logs a summary
once a minute (`hook calls in last 60s: …`), which shows whether hooks are firing.

## Game values

GameMaker values are `RValue`s, 16 bytes: an 8-byte payload, 4-byte flags, and a 4-byte
type tag, of which the low 24 bits count. Common tags: 0 real (f64), 1 string, 2 array,
5 undefined, 6 struct/object, 7 int32, 10 int64, 13 bool, 15 reference, `0xFFFFFF`
unset. Strings, arrays and structs are reference-counted, so NDML never overwrites them
directly; the SDK only writes numbers.

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

## Robustness and safety

* Mod callbacks run inside `catch_unwind`. A panicking mod is logged and the game
  continues; the original function still runs.
* The original game function is always called outside `catch_unwind`, through
  `extern "C-unwind"` functions, so the runtime's own exceptions propagate normally.
* `unsafe` code is limited to the FFI boundary: the loader's `entry`, `win`, `hooks`,
  `host` and `proxy` modules and the SDK's `ffi` module. Every other module, the
  manager, `ndml-common` and all included mods are `#![forbid(unsafe_code)]`.
* NDML makes no network connections and writes only below the game folder. Mods are
  native code and are not sandboxed: a mod can do anything the game process can.

## Limitations

* Mods cannot call GML functions or GameMaker built-in functions yet.
* Only numeric values can be written; built-ins and variables are read as numbers.
* A mod can access the hooked call's `self`/`other` instance and the global instance.
  State of other objects is obtained by hooking an event of that object and caching what
  is needed.
* At most 128 hooked functions per kind (events, scripts).
