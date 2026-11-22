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

//! Minimal PE header parsing for the already-mapped game image.

#![forbid(unsafe_code)]

/// A section as mapped in memory (addresses are RVAs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub rva: usize,
    pub size: usize,
}

impl Section {
    #[must_use]
    pub fn contains_rva(&self, rva: usize) -> bool {
        rva >= self.rva && rva < self.rva + self.size
    }
}

/// The bits of the PE headers the loader needs.
#[derive(Clone, Debug)]
pub struct Layout {
    pub entry_rva: usize,
    pub size_of_image: usize,
    pub sections: Vec<Section>,
}

impl Layout {
    #[must_use]
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

/// Size of one x64 `RUNTIME_FUNCTION` entry in `.pdata`: `{ begin, end, unwind }` RVAs.
const RUNTIME_FUNCTION_SIZE: usize = 12;

/// Bounds `[begin, end)` (RVAs) of the function containing `rva`, from the
/// exception table (`.pdata`) of a mapped image (`image[rva]` = byte at `rva`).
/// The table is sorted by begin RVA, as the PE format requires. `None` if there
/// is no `.pdata` or no entry covers `rva` (leaf functions have none).
#[must_use]
pub fn function_bounds(image: &[u8], layout: &Layout, rva: usize) -> Option<(usize, usize)> {
    let pdata = layout.section(".pdata")?;
    let table = image.get(pdata.rva..pdata.rva + pdata.size)?;
    let n = table.len() / RUNTIME_FUNCTION_SIZE;
    let entry = |i: usize| -> Option<(usize, usize)> {
        let o = i * RUNTIME_FUNCTION_SIZE;
        Some((u32_at(table, o)? as usize, u32_at(table, o + 4)? as usize))
    };
    // Last entry with begin <= rva (zero padding at the end sorts as begin 0,
    // so search only the non-zero prefix).
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let (begin, _) = entry(mid)?;
        if begin != 0 && begin <= rva {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let (begin, end) = entry(lo.checked_sub(1)?)?;
    (begin <= rva && rva < end).then_some((begin, end))
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Parse PE32+ headers from the first bytes of a mapped image.
pub fn parse(headers: &[u8]) -> Result<Layout, &'static str> {
    if headers.get(0..2) != Some(b"MZ") {
        return Err("missing MZ signature");
    }
    let nt = u32_at(headers, 0x3c).ok_or("truncated DOS header")? as usize;
    if headers.get(nt..nt + 4) != Some(b"PE\0\0") {
        return Err("missing PE signature");
    }
    let file_hdr = nt + 4;
    let n_sections = u16_at(headers, file_hdr + 2).ok_or("truncated file header")? as usize;
    let opt_size = u16_at(headers, file_hdr + 16).ok_or("truncated file header")? as usize;
    let opt = file_hdr + 20;
    if u16_at(headers, opt) != Some(0x20b) {
        return Err("not a PE32+ image");
    }
    let entry_rva = u32_at(headers, opt + 16).ok_or("truncated optional header")? as usize;
    let size_of_image = u32_at(headers, opt + 56).ok_or("truncated optional header")? as usize;
    let mut sections = Vec::with_capacity(n_sections);
    for i in 0..n_sections {
        let s = opt + opt_size + i * 40;
        let raw_name = headers.get(s..s + 8).ok_or("truncated section table")?;
        let name_len = raw_name.iter().position(|&c| c == 0).unwrap_or(8);
        sections.push(Section {
            name: String::from_utf8_lossy(&raw_name[..name_len]).into_owned(),
            size: u32_at(headers, s + 8).ok_or("truncated section table")? as usize,
            rva: u32_at(headers, s + 12).ok_or("truncated section table")? as usize,
        });
    }
    Ok(Layout {
        entry_rva,
        size_of_image,
        sections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_own_test_binary() {
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let layout = parse(&exe[..4096]).unwrap();
        assert!(layout.section(".text").is_some());
        assert!(layout.entry_rva > 0);
    }

    #[test]
    fn function_bounds_from_pdata() {
        let layout = Layout {
            entry_rva: 0,
            size_of_image: 0x200,
            sections: vec![Section {
                name: ".pdata".into(),
                rva: 0x100,
                size: 0x30,
            }],
        };
        let mut img = vec![0u8; 0x200];
        for (i, (b, e)) in [(0x10u32, 0x20u32), (0x20, 0x48), (0x50, 0x60)]
            .iter()
            .enumerate()
        {
            let o = 0x100 + i * 12;
            img[o..o + 4].copy_from_slice(&b.to_le_bytes());
            img[o + 4..o + 8].copy_from_slice(&e.to_le_bytes());
        }
        assert_eq!(function_bounds(&img, &layout, 0x10), Some((0x10, 0x20)));
        assert_eq!(function_bounds(&img, &layout, 0x47), Some((0x20, 0x48)));
        assert_eq!(function_bounds(&img, &layout, 0x4c), None); // gap
        assert_eq!(function_bounds(&img, &layout, 0x5f), Some((0x50, 0x60)));
        assert_eq!(function_bounds(&img, &layout, 0x08), None);
        assert_eq!(function_bounds(&img, &layout, 0x70), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"not a pe").is_err());
    }
}
