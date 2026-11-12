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
    Ok(index)
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
}
