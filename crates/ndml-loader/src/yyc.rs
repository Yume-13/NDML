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

//! Discovery of GameMaker YYC runtime tables inside the game executable.
//!
//! A YYC build compiles every GML script/event to a native function but keeps
//! two name-keyed tables in `.data` (see `docs/how-it-works.md`):
//!
//! * **Function table** — 24-byte entries `{ const char* name; void* func; YYVAR* var; }`
//!   where `name` starts with `gml_` and lives in `.rdata`, `func` is in `.text`.
//! * **Variable table** — 16-byte `YYVAR { const char* name; int32 slot; }`.
//!   `slot` is `-1` on disk and assigned by the runner during start-up; compiled
//!   code reads it on every variable access.
//!
//! Both are located by shape rather than by fixed address, so the scan does
//! not depend on a specific game build.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::pe::{Layout, Section};

/// A mapped image: `bytes[rva]` is the byte at `base + rva`.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    pub base: usize,
    pub bytes: &'a [u8],
}

impl Image<'_> {
    fn qword(&self, rva: usize) -> Option<usize> {
        let b = self.bytes.get(rva..rva + 8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?) as usize)
    }

    fn dword(&self, rva: usize) -> Option<u32> {
        let b = self.bytes.get(rva..rva + 4)?;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }

    fn rva_of(&self, addr: usize, section: &Section) -> Option<usize> {
        let rva = addr.checked_sub(self.base)?;
        section.contains_rva(rva).then_some(rva)
    }

    /// NUL-terminated ASCII string at `rva` (max 256 bytes).
    fn c_str(&self, rva: usize) -> Option<&str> {
        let tail = self.bytes.get(rva..)?;
        let len = tail.iter().take(256).position(|&c| c == 0)?;
        std::str::from_utf8(&tail[..len]).ok()
    }
}

/// Name → address maps for GML functions and variable slots.
#[derive(Debug, Default)]
pub struct RunnerIndex {
    /// `gml_*` name → absolute address of the compiled function.
    pub functions: HashMap<String, usize>,
    /// Variable name → absolute address of its runtime `int32` slot.
    pub variables: HashMap<String, usize>,
    /// Names seen more than once (first occurrence kept). Diagnostic only.
    pub duplicates: usize,
    /// Absolute address of the `.data` pointer to the GML global instance
    /// (`global.*` variables), if found. Read it at use time.
    pub global_instance_ptr: Option<usize>,
    /// Absolute address of the runner's `RValue` release routine
    /// (`void FREE_RValue(RValue*)`), if found. See [`find_rvalue_free`].
    pub rvalue_free: Option<usize>,
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Scan `.data` for the function and variable tables.
pub fn scan(image: Image<'_>, layout: &Layout) -> Result<RunnerIndex, &'static str> {
    let text = layout.section(".text").ok_or("no .text section")?;
    let rdata = layout.section(".rdata").ok_or("no .rdata section")?;
    let data = layout.section(".data").ok_or("no .data section")?;

    let mut index = RunnerIndex::default();
    let end = data.rva + data.size;
    let mut rva = data.rva;
    while rva + 16 <= end {
        let entry = rva;
        rva += 8;
        let Some(name_rva) = image.qword(entry).and_then(|p| image.rva_of(p, rdata)) else {
            continue;
        };
        let Some(name) = image.c_str(name_rva) else {
            continue;
        };

        // Function table entry?
        if name.starts_with("gml_") && entry + 24 <= end {
            let func = image
                .qword(entry + 8)
                .filter(|&p| image.rva_of(p, text).is_some());
            let var = image
                .qword(entry + 16)
                .filter(|&p| image.rva_of(p, data).is_some());
            if let (Some(func), Some(_)) = (func, var) {
                if index.functions.insert(name.to_owned(), func).is_some() {
                    index.duplicates += 1;
                }
                continue;
            }
        }
        // Unassigned YYVAR?
        if image.dword(entry + 8) == Some(u32::MAX) && is_identifier(name) {
            let slot_addr = image.base + entry + 8;
            if index.variables.insert(name.to_owned(), slot_addr).is_some() {
                index.duplicates += 1;
            }
        }
    }
    if index.functions.is_empty() {
        return Err("no GML function table found (not a YYC GameMaker build?)");
    }
    index.global_instance_ptr = find_global_instance_ptr(image, text, data);
    index.rvalue_free = find_rvalue_free(image, text);
    Ok(index)
}

/// The release routine is called from thousands of sites; demand a clear winner.
const MIN_FREE_SITES: usize = 500;

/// Find the runner's `RValue` release routine by how compiled code guards it:
/// `test r8, 0x46` (`(1 << kind) & 0x46`, i.e. string/array/object) then `je`
/// over a `call`. The most common call target after that guard wins if it has at
/// least `MIN_FREE_SITES` sites, twice as many as the runner-up (the
/// copy-a-reference helper is guarded the same way), and starts by masking the
/// kind (`and eax, 0xffffff`).
#[must_use]
pub fn find_rvalue_free(image: Image<'_>, text: &Section) -> Option<usize> {
    let bytes = image.bytes.get(text.rva..text.rva + text.size)?;
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let n = bytes.len();
    for i in 0..n.saturating_sub(24) {
        // `test r/m8, imm8` with a register operand (F6 C0+r) or `test al, imm8` (A8).
        let guard = match bytes[i] {
            0xF6 if bytes[i + 1] & 0xF8 == 0xC0 && bytes[i + 2] == 0x46 => 3,
            0xA8 if bytes[i + 1] == 0x46 => 2,
            _ => continue,
        };
        if bytes[i + guard] != 0x74 {
            continue; // je rel8
        }
        // First `call rel32` in the skipped block (it holds `mov rcx, …; call`).
        let start = i + guard + 2;
        let skip = usize::from(bytes[i + guard + 1]);
        let Some(call) = (start..(start + skip).min(n - 5)).find(|&j| bytes[j] == 0xE8) else {
            continue;
        };
        let rel = i32::from_le_bytes(bytes[call + 1..call + 5].try_into().ok()?);
        let Some(target) = (text.rva + call + 5).checked_add_signed(rel as isize) else {
            continue;
        };
        if text.contains_rva(target) {
            *counts.entry(target).or_default() += 1;
        }
    }
    let mut ranked: Vec<(usize, usize)> = counts.into_iter().collect();
    ranked.sort_unstable_by_key(|r| core::cmp::Reverse(r.1));
    let (best, n_best) = *ranked.first()?;
    let n_second = ranked.get(1).map_or(0, |r| r.1);
    let masks_kind = image
        .bytes
        .get(best..best + 32)?
        .windows(5)
        .any(|w| w == [0x25, 0xFF, 0xFF, 0xFF, 0x00]);
    (n_best >= MIN_FREE_SITES && n_best >= 2 * n_second && masks_kind).then_some(image.base + best)
}

/// Size of one entry of the runner's builtin function table (`RFunction`).
pub const FUNCTION_ENTRY_SIZE: usize = 24;
/// The runner registers a few thousand builtin functions (2,774 in Nova Drift
/// 1.2.38.4); fewer than this is not the table.
const MIN_FUNCTIONS: u32 = 500;
const MAX_FUNCTIONS: u32 = 1_000_000;
/// The table grows in steps (500 in this build); allow generous slack.
const MAX_FUNCTION_SLACK: u32 = 100_000;

/// Candidate locations of the builtin function table header
/// `{ RFunction* entries; int32 count; int32 capacity; }` in a runtime copy of
/// `.data` (`data[i]` is the byte at absolute `data_addr + i`). Returns the
/// absolute address of each header whose pointer is a heap address (outside the
/// image `image`) and whose count/capacity are plausible. The table is filled at
/// runner start-up (`Function_Add`), so this only works once the game runs.
#[must_use]
pub fn function_table_candidates(
    data: &[u8],
    data_addr: usize,
    image: &core::ops::Range<usize>,
) -> Vec<usize> {
    let mut out = Vec::new();
    let first = (8 - data_addr % 8) % 8;
    let mut off = first;
    while off + 16 <= data.len() {
        let q = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap_or([0; 8])) as usize;
        let d = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap_or([0; 4]));
        let (ptr, count, cap) = (q(off), d(off + 8), d(off + 12));
        if ptr != 0
            && ptr % 8 == 0
            && ptr < 0x0000_8000_0000_0000
            && !image.contains(&ptr)
            && (MIN_FUNCTIONS..=MAX_FUNCTIONS).contains(&count)
            && count <= cap
            && cap - count <= MAX_FUNCTION_SLACK
        {
            out.push(data_addr + off);
        }
        off += 8;
    }
    out
}

/// One builtin function table entry (`RFunction`):
/// `{ const char* name; void (*routine)(RValue* result, CInstance* self,
/// CInstance* other, int argc, RValue* args); int32 argc; … }`, 24 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionEntry {
    /// Absolute address of the NUL-terminated name.
    pub name: usize,
    /// Absolute address of the routine (in `.text`).
    pub routine: usize,
    /// Declared argument count (`-1` = variadic).
    pub argc: i32,
}

/// Parse a copy of the builtin function table. Entries that don't look like an
/// `RFunction` (null name, routine outside `.text`) are `None`, keeping the
/// indices aligned with the runner's function ids. `None` overall if fewer
/// than 90% of the entries are well-formed (not the table).
#[must_use]
pub fn parse_function_entries(
    bytes: &[u8],
    text: &core::ops::Range<usize>,
) -> Option<Vec<Option<FunctionEntry>>> {
    let entries: Vec<Option<FunctionEntry>> = bytes
        .as_chunks::<FUNCTION_ENTRY_SIZE>()
        .0
        .iter()
        .map(|e| {
            let name = u64::from_le_bytes(e[0..8].try_into().ok()?) as usize;
            let routine = u64::from_le_bytes(e[8..16].try_into().ok()?) as usize;
            let argc = i32::from_le_bytes(e[16..20].try_into().ok()?);
            (name != 0 && !text.contains(&name) && text.contains(&routine)).then_some(
                FunctionEntry {
                    name,
                    routine,
                    argc,
                },
            )
        })
        .collect();
    let good = entries.iter().filter(|e| e.is_some()).count();
    (!entries.is_empty() && good * 10 >= entries.len() * 9).then_some(entries)
}

/// String-returning builtins whose routines all create their result with the
/// runner's create-string routine. Used to find that routine by name.
pub const STRING_BUILTINS: &[&str] = &[
    "chr",
    "ansi_char",
    "string_upper",
    "string_lower",
    "string_copy",
    "string_delete",
    "string_insert",
    "string_replace",
    "string_replace_all",
    "string_repeat",
    "string_letters",
    "string_digits",
    "string_lettersdigits",
    "string_char_at",
    "filename_name",
    "filename_path",
    "filename_ext",
    "filename_dir",
    "filename_drive",
];

/// A candidate must be called by at least this many of the routines.
const MIN_STRING_CALLERS: usize = 3;
/// The create-string routine is small (about 230 bytes in 1.2.38.4).
const MAX_CREATE_STRING_SIZE: usize = 0x400;

/// Candidates for the runner's create-string routine
/// (`void YYCreateString(RValue* out, const char* s)`, which copies `s`), most
/// likely first. `routines` are the absolute addresses of the [`STRING_BUILTINS`]
/// routines found in the runtime builtin table. A candidate is a direct call
/// target shared by at least `MIN_STRING_CALLERS` of them whose body has the
/// create-string shape (see [`looks_like_create_string`]). The caller must still
/// verify the winner at runtime before trusting it.
#[must_use]
pub fn create_string_candidates(
    image: Image<'_>,
    layout: &Layout,
    routines: &[usize],
) -> Vec<usize> {
    let Some(text) = layout.section(".text") else {
        return Vec::new();
    };
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &routine in routines {
        let Some(rva) = routine.checked_sub(image.base) else {
            continue;
        };
        let Some((begin, end)) = crate::pe::function_bounds(image.bytes, layout, rva) else {
            continue;
        };
        let Some(body) = image.bytes.get(begin..end) else {
            continue;
        };
        let mut targets: Vec<usize> = Vec::new();
        for j in 0..body.len().saturating_sub(5) {
            if body[j] != 0xE8 {
                continue;
            }
            let Ok(rel) = body[j + 1..j + 5].try_into().map(i32::from_le_bytes) else {
                continue;
            };
            let Some(t) = (begin + j + 5).checked_add_signed(rel as isize) else {
                continue;
            };
            // Only calls to the start of a known function.
            if text.contains_rva(t)
                && crate::pe::function_bounds(image.bytes, layout, t).is_some_and(|b| b.0 == t)
                && !targets.contains(&t)
            {
                targets.push(t);
            }
        }
        for t in targets {
            *counts.entry(t).or_default() += 1;
        }
    }
    let mut ranked: Vec<(usize, usize)> = counts
        .into_iter()
        .filter(|&(_, n)| n >= MIN_STRING_CALLERS)
        .collect();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .filter(|&(t, _)| {
            crate::pe::function_bounds(image.bytes, layout, t)
                .filter(|(b, e)| e - b <= MAX_CREATE_STRING_SIZE)
                .and_then(|(b, e)| image.bytes.get(b..e))
                .is_some_and(looks_like_create_string)
        })
        .map(|(t, _)| image.base + t)
        .collect()
}

/// `mov dword [reg+disp8], imm32` (optionally REX-prefixed): `C7 /0` with
/// `mod = 01`.
fn stores_dword(body: &[u8], disp: u8, imm: u32) -> bool {
    let imm = imm.to_le_bytes();
    body.windows(7).any(|w| {
        w[0] == 0xC7 && w[1] & 0xF8 == 0x40 && w[1] & 0x07 != 0x04 && w[2] == disp && w[3..7] == imm
    })
}

/// Shape of a create-string routine: stores `kind = 1` (string) at `+0x0C` of
/// the output `RValue` and `refcount = 1` at `+0x08` of the new `RefString`,
/// and does **not** tag the length with `0x80000000` (that marks the
/// non-copying "constant string" variant, whose text must outlive the value).
#[must_use]
pub fn looks_like_create_string(body: &[u8]) -> bool {
    let tags_constant = body
        .windows(6)
        .any(|w| w[0] == 0x81 && w[1] & 0xF8 == 0xC8 && w[2..6] == [0, 0, 0, 0x80])
        || body.windows(5).any(|w| w == [0x0D, 0, 0, 0, 0x80]);
    stores_dword(body, 0x0C, 1) && stores_dword(body, 0x08, 1) && !tags_constant
}

/// Global-variable accesses need at least this many sites to trust the result.
const MIN_GLOBAL_SITES: usize = 100;

/// Find the pointer to the global instance by how compiled GML reads globals:
/// `mov rcx, [rip+G]` (`48 8B 0D rel32`) then `mov r8, [rcx]` (`4C 8B 01`, its
/// vtable) before calling a variable accessor. Thousands of sites share one
/// `G` in `.data`; the most common target wins.
fn find_global_instance_ptr(image: Image<'_>, text: &Section, data: &Section) -> Option<usize> {
    let bytes = image.bytes.get(text.rva..text.rva + text.size)?;
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for i in 0..bytes.len().saturating_sub(10) {
        if bytes[i..i + 3] != [0x48, 0x8B, 0x0D] || bytes[i + 7..i + 10] != [0x4C, 0x8B, 0x01] {
            continue;
        }
        let rel = i32::from_le_bytes(bytes[i + 3..i + 7].try_into().ok()?);
        let next = text.rva + i + 7;
        let Some(target) = next.checked_add_signed(rel as isize) else {
            continue;
        };
        if data.contains_rva(target) {
            *counts.entry(target).or_default() += 1;
        }
    }
    let (rva, n) = counts.into_iter().max_by_key(|&(_, n)| n)?;
    (n >= MIN_GLOBAL_SITES).then_some(image.base + rva)
}

/// Size of one entry in the runner's built-in variable table.
pub const BUILTIN_ENTRY_SIZE: usize = 32;
/// The runner registers a few hundred built-ins (max 500); a shorter run of
/// table-shaped entries is something else.
const MIN_BUILTIN_RUN: usize = 64;

/// One entry of the runner's built-in variable table (filled at start-up, in
/// `.data`/.bss): `{ char* name (heap); getter; setter (null = read-only);
/// bool can_set; }`, 32 bytes. See `docs/how-it-works.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinEntry {
    /// Absolute address of the (heap-owned) NUL-terminated name.
    pub name: usize,
    /// `bool getter(CInstance*, int array_index, RValue* out)`.
    pub getter: usize,
    /// `bool setter(CInstance*, int array_index, RValue* value)`, if writable.
    pub setter: Option<usize>,
}

fn builtin_entry(e: &[u8], text: &core::ops::Range<usize>) -> Option<BuiltinEntry> {
    let q = |o: usize| -> Option<usize> {
        Some(u64::from_le_bytes(e.get(o..o + 8)?.try_into().ok()?) as usize)
    };
    let (name, getter, setter) = (q(0)?, q(8)?, q(16)?);
    let can_set = *e.get(24)?;
    let padding_clear = e.get(25..32)?.iter().all(|&b| b == 0);
    let ok = name != 0
        && !text.contains(&name)
        && text.contains(&getter)
        && (setter == 0 || text.contains(&setter))
        && can_set == u8::from(setter != 0)
        && padding_clear;
    ok.then_some(BuiltinEntry {
        name,
        getter,
        setter: (setter != 0).then_some(setter),
    })
}

/// Find the built-in variable table by shape in a runtime copy of `.data`
/// (`data[i]` is the byte at absolute address `data_addr + i`): the longest run
/// of 32-byte-aligned entries matching [`BuiltinEntry`], at least
/// `MIN_BUILTIN_RUN` long. `text` is the absolute `.text` range.
#[must_use]
pub fn find_builtin_table(
    data: &[u8],
    data_addr: usize,
    text: &core::ops::Range<usize>,
) -> Option<Vec<BuiltinEntry>> {
    let first = (BUILTIN_ENTRY_SIZE - data_addr % BUILTIN_ENTRY_SIZE) % BUILTIN_ENTRY_SIZE;
    let mut best: Vec<BuiltinEntry> = Vec::new();
    let mut run: Vec<BuiltinEntry> = Vec::new();
    let mut off = first;
    while off + BUILTIN_ENTRY_SIZE <= data.len() {
        match builtin_entry(&data[off..off + BUILTIN_ENTRY_SIZE], text) {
            Some(e) => run.push(e),
            None => {
                if run.len() > best.len() {
                    best = std::mem::take(&mut run);
                } else {
                    run.clear();
                }
            }
        }
        off += BUILTIN_ENTRY_SIZE;
    }
    if run.len() > best.len() {
        best = run;
    }
    (best.len() >= MIN_BUILTIN_RUN).then_some(best)
}

/// The runner's `with` routines, as compiled GML calls them:
/// `int begin(iter*, CInstance** self, CInstance** other, RValue* target)`,
/// `bool next(iter*, CInstance** self, CInstance** other)` and
/// `void end(iter*, CInstance** self, CInstance** other)` (restores `self`/`other`
/// and frees the instance list). Absolute addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithRoutines {
    pub begin: usize,
    pub next: usize,
    pub end: usize,
}

/// Votes a routine needs before it is trusted.
const MIN_WITH_SITES: usize = 20;

/// `lea reg, [rbp+disp]` (REX `rex`, `ModRM` `modrm8` for disp8 or `modrm32` for
/// disp32) ending exactly at `end`: `(disp, start)`.
fn lea_rbp_ending_at(
    body: &[u8],
    end: usize,
    rex: u8,
    modrm8: u8,
    modrm32: u8,
) -> Option<(i32, usize)> {
    if let Some(s) = end.checked_sub(7)
        && body.get(s..s + 3) == Some(&[rex, 0x8D, modrm32][..])
    {
        return Some((
            i32::from_le_bytes(body.get(s + 3..s + 7)?.try_into().ok()?),
            s,
        ));
    }
    let s = end.checked_sub(4)?;
    if body.get(s..s + 3) == Some(&[rex, 0x8D, modrm8][..]) {
        return Some((i32::from(*body.get(s + 3)? as i8), s));
    }
    None
}

/// `lea reg, [rbp+disp]` starting at `start`: `(disp, length)`.
fn lea_rbp_at(body: &[u8], start: usize, rex: u8, modrm8: u8, modrm32: u8) -> Option<(i32, usize)> {
    match body.get(start..start + 3)? {
        [r, 0x8D, m] if *r == rex && *m == modrm32 => Some((
            i32::from_le_bytes(body.get(start + 3..start + 7)?.try_into().ok()?),
            7,
        )),
        [r, 0x8D, m] if *r == rex && *m == modrm8 => {
            Some((i32::from(*body.get(start + 3)? as i8), 4))
        }
        _ => None,
    }
}

/// `Function_Add` is called once per builtin; demand a clear winner.
const MIN_FUNCTION_ADD_SITES: usize = 1000;
/// How far before the `call` the two `lea`s may be.
const REGISTRATION_WINDOW: usize = 40;

/// Builtin function name → absolute routine address, recovered statically from
/// the runner's registrations `Function_Add(name, routine, argc)` (compiled as
/// `lea rcx, [rip+name]; lea rdx, [rip+routine]; …; call Function_Add`).
/// `Function_Add` is the call target most often preceded by both `lea`s with
/// `rcx` an identifier in `.rdata` and `rdx` in `.text` (at least
/// `MIN_FUNCTION_ADD_SITES`). Works at boot, before the runner fills its
/// table. First registration wins for a name registered twice.
#[must_use]
pub fn find_builtin_routines(image: Image<'_>, layout: &Layout) -> Option<HashMap<String, usize>> {
    let text = layout.section(".text")?;
    let rdata = layout.section(".rdata")?;
    let bytes = image.bytes.get(text.rva..text.rva + text.size)?;
    let lea_target = |at: usize, modrm: u8| -> Option<usize> {
        if bytes.get(at..at + 3)? != [0x48, 0x8D, modrm] {
            return None;
        }
        let rel = i32::from_le_bytes(bytes.get(at + 3..at + 7)?.try_into().ok()?);
        (text.rva + at + 7).checked_add_signed(rel as isize)
    };
    // (call target, name rva, routine rva) for every call with the shape.
    let mut sites = Vec::new();
    for i in REGISTRATION_WINDOW..bytes.len().saturating_sub(5) {
        if bytes[i] != 0xE8 {
            continue;
        }
        let rel = i32::from_le_bytes(bytes[i + 1..i + 5].try_into().ok()?);
        let Some(target) = (text.rva + i + 5).checked_add_signed(rel as isize) else {
            continue;
        };
        if !text.contains_rva(target) {
            continue;
        }
        let (mut name, mut routine) = (None, None);
        for j in i - REGISTRATION_WINDOW..i {
            if let Some(t) = lea_target(j, 0x0D).filter(|&t| rdata.contains_rva(t)) {
                name = Some(t);
            }
            if let Some(t) = lea_target(j, 0x15).filter(|&t| text.contains_rva(t)) {
                routine = Some(t);
            }
        }
        if let (Some(n), Some(r)) = (name, routine) {
            sites.push((target, n, r));
        }
    }
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for &(t, ..) in &sites {
        *counts.entry(t).or_default() += 1;
    }
    let (&add, &n) = counts
        .iter()
        .max_by_key(|&(t, n)| (*n, core::cmp::Reverse(*t)))?;
    if n < MIN_FUNCTION_ADD_SITES {
        return None;
    }
    let mut map = HashMap::new();
    for (t, name, routine) in sites {
        if t != add {
            continue;
        }
        if let Some(s) = image.c_str(name).filter(|s| is_identifier(s)) {
            map.entry(s.to_owned()).or_insert(image.base + routine);
        }
    }
    Some(map)
}

/// Find the `with` routines by how compiled code calls them. Each is called
/// right after `lea r8,[rbp+s+8]; lea rdx,[rbp+s]; lea rcx,[rbp+i]` (the
/// `&other`/`&self` slots and the iterator), and the call is classified by
/// what follows it: `begin`'s count is tested (`test eax,eax` within 6 bytes),
/// `next`'s bool directly (`test al,al`), and `end` is followed by `lea
/// rcx,[rbp+i]` and the call that frees the iterator. The most voted target of
/// each kind wins if it has `MIN_WITH_SITES` votes and the three differ.
/// `functions` are absolute addresses of compiled GML functions.
#[must_use]
pub fn find_with_routines(
    image: Image<'_>,
    layout: &Layout,
    functions: &[usize],
) -> Option<WithRoutines> {
    let text = layout.section(".text")?;
    let mut votes: [HashMap<usize, usize>; 3] = Default::default();
    for &f in functions {
        let Some(rva) = f.checked_sub(image.base) else {
            continue;
        };
        let Some((begin, end)) = crate::pe::function_bounds(image.bytes, layout, rva) else {
            continue;
        };
        let Some(body) = image.bytes.get(begin..end) else {
            continue;
        };
        for j in 0..body.len().saturating_sub(5) {
            if let Some((kind, t)) = classify_with_call(body, j)
                && let Some(t) = begin.checked_add_signed(t)
                && text.contains_rva(t)
            {
                *votes[kind].entry(t).or_default() += 1;
            }
        }
    }
    let best = |m: &HashMap<usize, usize>| {
        m.iter()
            .max_by_key(|&(t, n)| (*n, std::cmp::Reverse(*t)))
            .filter(|&(_, n)| *n >= MIN_WITH_SITES)
            .map(|(&t, _)| image.base + t)
    };
    let r = WithRoutines {
        begin: best(&votes[0])?,
        next: best(&votes[1])?,
        end: best(&votes[2])?,
    };
    (r.begin != r.next && r.next != r.end && r.begin != r.end).then_some(r)
}

/// If `body[j]` is a `call rel32` preceded by the `with` argument setup, its
/// kind (0 begin, 1 next, 2 end) and target offset relative to `body`'s start.
fn classify_with_call(body: &[u8], j: usize) -> Option<(usize, isize)> {
    if body.get(j) != Some(&0xE8) {
        return None;
    }
    let (iter, rcx_start) = lea_rbp_ending_at(body, j, 0x48, 0x4D, 0x8D)?;
    let (slot, dx_start) = lea_rbp_ending_at(body, rcx_start, 0x48, 0x55, 0x95)?;
    let (other, _) = lea_rbp_ending_at(body, dx_start, 0x4C, 0x45, 0x85)?;
    if other != slot.wrapping_add(8) {
        return None;
    }
    let rel = i32::from_le_bytes(body.get(j + 1..j + 5)?.try_into().ok()?);
    let target = (j + 5) as isize + rel as isize;
    let after = body.get(j + 5..body.len().min(j + 11))?;
    let kind = if after.starts_with(&[0x84, 0xC0]) {
        1
    } else if after.windows(2).any(|w| w == [0x85, 0xC0]) {
        0
    } else {
        let k = j + 5 + usize::from(body.get(j + 5) == Some(&0x90));
        match lea_rbp_at(body, k, 0x48, 0x4D, 0x8D) {
            Some((d, len)) if d == iter && body.get(k + len) == Some(&0xE8) => 2,
            _ => return None,
        }
    };
    Some((kind, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: usize = 0x1_4000_0000;

    fn put_q(img: &mut [u8], rva: usize, v: usize) {
        img[rva..rva + 8].copy_from_slice(&(v as u64).to_le_bytes());
    }

    fn put_s(img: &mut [u8], rva: usize, s: &str) {
        img[rva..rva + s.len()].copy_from_slice(s.as_bytes());
    }

    fn synthetic() -> (Vec<u8>, Layout) {
        let sec = |name: &str, rva, size| Section {
            name: name.into(),
            rva,
            size,
        };
        let layout = Layout {
            entry_rva: 0x1000,
            size_of_image: 0x4000,
            sections: vec![
                sec(".text", 0x1000, 0x1000),
                sec(".rdata", 0x2000, 0x1000),
                sec(".data", 0x3000, 0x1000),
            ],
        };
        let mut img = vec![0u8; 0x4000];
        put_s(&mut img, 0x2000, "gml_Script_powerupAttract");
        put_s(&mut img, 0x2100, "magDist");
        put_s(&mut img, 0x2200, "not an ident");
        // function entry
        put_q(&mut img, 0x3000, BASE + 0x2000);
        put_q(&mut img, 0x3008, BASE + 0x1234);
        put_q(&mut img, 0x3010, BASE + 0x3100);
        // YYVAR magDist
        put_q(&mut img, 0x3040, BASE + 0x2100);
        img[0x3048..0x304c].copy_from_slice(&u32::MAX.to_le_bytes());
        // YYVAR with a bad name is ignored
        put_q(&mut img, 0x3060, BASE + 0x2200);
        img[0x3068..0x306c].copy_from_slice(&u32::MAX.to_le_bytes());
        (img, layout)
    }

    #[test]
    fn finds_functions_and_variables() {
        let (img, layout) = synthetic();
        let idx = scan(
            Image {
                base: BASE,
                bytes: &img,
            },
            &layout,
        )
        .unwrap();
        assert_eq!(idx.functions["gml_Script_powerupAttract"], BASE + 0x1234);
        assert_eq!(idx.variables["magDist"], BASE + 0x3048);
        assert_eq!(idx.variables.len(), 1);
    }

    #[test]
    fn errors_without_function_table() {
        let (mut img, layout) = synthetic();
        img[0x3000..0x3018].fill(0);
        assert!(
            scan(
                Image {
                    base: BASE,
                    bytes: &img
                },
                &layout
            )
            .is_err()
        );
    }

    fn builtin_bytes(n: usize, data_addr: usize) -> Vec<u8> {
        // 16 bytes of junk, then n entries, then zeros.
        let mut d = vec![0xAAu8; 16];
        for i in 0..n {
            let mut e = [0u8; 32];
            e[0..8].copy_from_slice(&(0x5_0000 + i as u64 * 0x10).to_le_bytes());
            e[8..16].copy_from_slice(&(BASE as u64 + 0x1100).to_le_bytes());
            if i % 3 != 0 {
                e[16..24].copy_from_slice(&(BASE as u64 + 0x1200).to_le_bytes());
                e[24] = 1;
            }
            d.extend_from_slice(&e);
        }
        d.extend_from_slice(&[0u8; 64]);
        assert_eq!((data_addr + 16) % 32, 0);
        d
    }

    #[test]
    fn finds_builtin_table() {
        let text = BASE + 0x1000..BASE + 0x2000;
        let addr = BASE + 0x3000 - 16;
        let d = builtin_bytes(100, addr);
        let t = find_builtin_table(&d, addr, &text).unwrap();
        assert_eq!(t.len(), 100);
        assert_eq!(t[0].name, 0x5_0000);
        assert_eq!(t[0].setter, None);
        assert_eq!(t[1].setter, Some(BASE + 0x1200));
    }

    #[test]
    fn rejects_short_or_inconsistent_runs() {
        let text = BASE + 0x1000..BASE + 0x2000;
        let addr = BASE + 0x3000 - 16;
        assert!(find_builtin_table(&builtin_bytes(10, addr), addr, &text).is_none());
        let mut d = builtin_bytes(100, addr);
        // can_set flag disagreeing with the setter breaks the run in the middle.
        d[16 + 50 * 32 + 24] = 0;
        assert!(find_builtin_table(&d, addr, &text).is_none());
    }

    #[test]
    fn finds_global_instance_pointer() {
        let (mut img, layout) = synthetic();
        // 120 global loads at 0x1100.. pointing at .data 0x3800, a few elsewhere.
        let site = |img: &mut Vec<u8>, at: usize, target: usize| {
            let rel = (target as isize - (at as isize + 7)) as i32;
            img[at..at + 3].copy_from_slice(&[0x48, 0x8B, 0x0D]);
            img[at + 3..at + 7].copy_from_slice(&rel.to_le_bytes());
            img[at + 7..at + 10].copy_from_slice(&[0x4C, 0x8B, 0x01]);
        };
        for k in 0..120 {
            site(&mut img, 0x1100 + k * 12, 0x3800);
        }
        site(&mut img, 0x1a00, 0x3900);
        let image = Image {
            base: BASE,
            bytes: &img,
        };
        let idx = scan(image, &layout).unwrap();
        assert_eq!(idx.global_instance_ptr, Some(BASE + 0x3800));
    }

    #[test]
    fn identifier_rules() {
        assert!(is_identifier("magDist"));
        assert!(is_identifier("_x1"));
        assert!(!is_identifier("1x"));
        assert!(!is_identifier("a b"));
        assert!(!is_identifier(""));
    }

    /// `call rel32` at `at` (an RVA) to `target`.
    fn put_call(img: &mut [u8], at: usize, target: usize) {
        let rel = (target as isize - (at as isize + 5)) as i32;
        img[at] = 0xE8;
        img[at + 1..at + 5].copy_from_slice(&rel.to_le_bytes());
    }

    #[test]
    fn finds_rvalue_free_by_guard_shape() {
        // `test dl, 0x46` / `test al, 0x46`; `je +8`; `mov rcx, rdi`; `call X`.
        let site = |img: &mut Vec<u8>, at: usize, target: usize, al: bool| {
            let g = if al {
                img[at..at + 2].copy_from_slice(&[0xA8, 0x46]);
                2
            } else {
                img[at..at + 3].copy_from_slice(&[0xF6, 0xC2, 0x46]);
                3
            };
            img[at + g..at + g + 2].copy_from_slice(&[0x74, 0x08]);
            img[at + g + 2..at + g + 5].copy_from_slice(&[0x48, 0x8B, 0xCF]);
            put_call(img, at + g + 5, target);
        };
        let mut img = vec![0u8; 0x40000];
        let text = Section {
            name: ".text".into(),
            rva: 0x1000,
            size: 0x3f000,
        };
        let find = |img: &[u8]| {
            find_rvalue_free(
                Image {
                    base: BASE,
                    bytes: img,
                },
                &text,
            )
        };
        let (free, copy) = (0x3e000usize, 0x3e100usize);
        img[free..free + 3].copy_from_slice(&[0x8B, 0x41, 0x0C]); // mov eax, [rcx+0xc]
        img[free + 3..free + 8].copy_from_slice(&[0x25, 0xFF, 0xFF, 0xFF, 0x00]);
        let mut at = 0x2000;
        let mut add = |img: &mut Vec<u8>, n: usize, target: usize| {
            for k in 0..n {
                site(img, at, target, k % 3 == 0);
                at += 16;
            }
        };
        add(&mut img, 10, free);
        assert_eq!(find(&img), None, "too few sites");
        add(&mut img, MIN_FREE_SITES, free);
        add(&mut img, 100, copy);
        assert_eq!(find(&img), Some(BASE + free));
        add(&mut img, MIN_FREE_SITES, copy);
        assert_eq!(find(&img), None, "runner-up too close");
    }

    #[test]
    fn finds_function_table_header() {
        let image = BASE..BASE + 0x4000;
        let data_addr = BASE + 0x3000;
        let mut d = vec![0u8; 0x100];
        // Decoy: count without a heap pointer.
        d[0x10..0x18].copy_from_slice(&(BASE as u64 + 0x3100).to_le_bytes());
        d[0x18..0x1c].copy_from_slice(&2774u32.to_le_bytes());
        d[0x1c..0x20].copy_from_slice(&3000u32.to_le_bytes());
        // Real header.
        d[0x40..0x48].copy_from_slice(&0x2_5000_0010u64.to_le_bytes());
        d[0x48..0x4c].copy_from_slice(&2774u32.to_le_bytes());
        d[0x4c..0x50].copy_from_slice(&3000u32.to_le_bytes());
        // Decoy: capacity below count.
        d[0x60..0x68].copy_from_slice(&0x2_5000_0010u64.to_le_bytes());
        d[0x68..0x6c].copy_from_slice(&2774u32.to_le_bytes());
        d[0x6c..0x70].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(
            function_table_candidates(&d, data_addr, &image),
            vec![data_addr + 0x40]
        );
    }

    #[test]
    fn parses_function_entries() {
        let text = BASE + 0x1000..BASE + 0x2000;
        let mut b = Vec::new();
        for i in 0..20u64 {
            b.extend_from_slice(&(0x2_6000_0000 + i * 32).to_le_bytes());
            b.extend_from_slice(&(BASE as u64 + 0x1000 + i * 16).to_le_bytes());
            b.extend_from_slice(&(i as i32 - 1).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
        }
        let e = parse_function_entries(&b, &text).unwrap();
        assert_eq!(e.len(), 20);
        assert_eq!(e[0].unwrap().argc, -1);
        assert_eq!(e[3].unwrap().routine, BASE + 0x1030);
        // One bad entry keeps its slot as None.
        b[8 * 24 + 8..8 * 24 + 16].copy_from_slice(&0u64.to_le_bytes());
        let e = parse_function_entries(&b, &text).unwrap();
        assert!(e[8].is_none() && e[9].is_some());
        // Mostly garbage is rejected.
        assert!(parse_function_entries(&[0u8; 24 * 10], &text).is_none());
    }

    /// Body bytes shaped like the runner's create-string routine (a minimal
    /// hand-written instruction sequence, not game code).
    fn create_string_body(constant: bool) -> Vec<u8> {
        let mut b = vec![0x48, 0x8B, 0xFA]; // mov rdi, rdx
        b.extend_from_slice(&[0xC7, 0x46, 0x08, 1, 0, 0, 0]); // mov [rsi+8], 1
        if constant {
            b.extend_from_slice(&[0x81, 0xCA, 0, 0, 0, 0x80]); // or edx, 0x80000000
        }
        b.extend_from_slice(&[0x41, 0xC7, 0x46, 0x0C, 1, 0, 0, 0]); // mov [r14+0xc], 1
        b.push(0xC3);
        b
    }

    #[test]
    fn create_string_shape() {
        assert!(looks_like_create_string(&create_string_body(false)));
        assert!(!looks_like_create_string(&create_string_body(true)));
        assert!(!looks_like_create_string(&[
            0xC7, 0x46, 0x08, 1, 0, 0, 0, 0xC3
        ]));
    }

    #[test]
    fn finds_create_string_among_shared_callees() {
        // .text 0x1000..0x3000, .pdata at 0x3000.
        let layout = Layout {
            entry_rva: 0x1000,
            size_of_image: 0x4000,
            sections: vec![
                Section {
                    name: ".text".into(),
                    rva: 0x1000,
                    size: 0x2000,
                },
                Section {
                    name: ".pdata".into(),
                    rva: 0x3000,
                    size: 0x100,
                },
            ],
        };
        let mut img = vec![0u8; 0x4000];
        let (getter, create) = (0x1800usize, 0x1900usize);
        let body = create_string_body(false);
        img[create..create + body.len()].copy_from_slice(&body);
        img[getter] = 0xC3;
        let mut funcs = vec![(getter, getter + 0x40), (create, create + 0x40)];
        // Four "string builtin" routines, each calling the getter and create.
        let routines: Vec<usize> = (0..4).map(|k| 0x1000 + k * 0x100).collect();
        for &r in &routines {
            put_call(&mut img, r + 0x10, getter);
            put_call(&mut img, r + 0x20, create);
            funcs.push((r, r + 0x80));
        }
        funcs.sort_unstable();
        for (i, (b, e)) in funcs.iter().enumerate() {
            let o = 0x3000 + i * 12;
            img[o..o + 4].copy_from_slice(&(*b as u32).to_le_bytes());
            img[o + 4..o + 8].copy_from_slice(&(*e as u32).to_le_bytes());
        }
        let image = Image {
            base: BASE,
            bytes: &img,
        };
        let abs: Vec<usize> = routines.iter().map(|r| BASE + r).collect();
        assert_eq!(
            create_string_candidates(image, &layout, &abs),
            vec![BASE + create]
        );
        // Called by too few routines: no candidate.
        assert!(create_string_candidates(image, &layout, &abs[..2]).is_empty());
    }

    /// `lea rcx,[name]; lea rdx,[routine]; mov r8d,n; call Function_Add`, as
    /// the runner registers its builtins.
    #[test]
    fn finds_builtin_registrations() {
        let sec = |name: &str, rva, size| Section {
            name: name.into(),
            rva,
            size,
        };
        let (text, rdata) = (0x1000usize, 0x10_0000usize);
        let layout = Layout {
            entry_rva: text,
            size_of_image: 0x12_0000,
            sections: vec![sec(".text", text, 0xF_0000), sec(".rdata", rdata, 0x2_0000)],
        };
        let mut img = vec![0u8; layout.size_of_image];
        let add = text + 0xE_0000; // Function_Add
        let rel = |at: usize, to: usize| ((to as isize - at as isize) as i32).to_le_bytes();
        let mut at = text + 0x100;
        for i in 0..1200usize {
            let name = rdata + i * 16;
            put_s(&mut img, name, &format!("fn_{i}"));
            let routine = text + 0xC_0000 + i * 16;
            img[at..at + 3].copy_from_slice(&[0x48, 0x8D, 0x0D]);
            img[at + 3..at + 7].copy_from_slice(&rel(at + 7, name));
            img[at + 7..at + 10].copy_from_slice(&[0x48, 0x8D, 0x15]);
            img[at + 10..at + 14].copy_from_slice(&rel(at + 14, routine));
            img[at + 14..at + 20].copy_from_slice(&[0x41, 0xB8, 2, 0, 0, 0]);
            img[at + 20] = 0xE8;
            img[at + 21..at + 25].copy_from_slice(&rel(at + 25, add));
            at += 25;
        }
        let map = find_builtin_routines(
            Image {
                base: BASE,
                bytes: &img,
            },
            &layout,
        );
        let map = map.unwrap_or_default();
        assert_eq!(map.len(), 1200);
        assert_eq!(map.get("fn_7"), Some(&(BASE + text + 0xC_0000 + 7 * 16)));
        // Too few registrations: not the table.
        let few = Layout {
            sections: vec![
                sec(".text", text, 0x100 + 25 * 500),
                sec(".rdata", rdata, 0x2_0000),
            ],
            ..layout
        };
        assert!(
            find_builtin_routines(
                Image {
                    base: BASE,
                    bytes: &img
                },
                &few
            )
            .is_none()
        );
    }

    /// The argument setup compiled `with` code uses, with the call classified
    /// by what follows it.
    #[test]
    fn classifies_with_calls() {
        // lea r8,[rbp+0xb58]; lea rdx,[rbp+0xb50]; lea rcx,[rbp+0x320]
        let setup: &[u8] = &[
            0x4C, 0x8D, 0x85, 0x58, 0x0B, 0, 0, 0x48, 0x8D, 0x95, 0x50, 0x0B, 0, 0, 0x48, 0x8D,
            0x8D, 0x20, 0x03, 0, 0,
        ];
        let call = |rel: i32| {
            let mut v = vec![0xE8];
            v.extend_from_slice(&rel.to_le_bytes());
            v
        };
        let site = |tail: &[u8]| {
            let mut b = setup.to_vec();
            b.extend(call(0x100));
            b.extend_from_slice(tail);
            b
        };
        let j = setup.len();
        let target = (j + 5) as isize + 0x100;
        // next: test al,al
        assert_eq!(
            classify_with_call(&site(&[0x84, 0xC0, 0x75, 0x00]), j),
            Some((1, target))
        );
        // begin: xor esi,esi; test eax,eax
        assert_eq!(
            classify_with_call(&site(&[0x33, 0xF6, 0x85, 0xC0, 0x7E, 0x00]), j),
            Some((0, target))
        );
        // end: nop; lea rcx,[rbp+0x320]; call free
        let mut end = vec![0x90, 0x48, 0x8D, 0x8D, 0x20, 0x03, 0, 0];
        end.extend(call(0x10));
        assert_eq!(classify_with_call(&site(&end), j), Some((2, target)));
        // Different iterator in the free call: not an end site.
        end[4] = 0x28;
        assert_eq!(classify_with_call(&site(&end), j), None);
        // Mismatched self/other slots: not a with call.
        let mut bad = site(&[0x84, 0xC0]);
        bad[3] = 0x60;
        assert_eq!(classify_with_call(&bad, j), None);
    }
}
