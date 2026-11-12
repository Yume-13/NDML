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

//! Mod metadata embedded in the DLL as plain data.
//!
//! [`crate::export_mod!`] emits an exported `NDML_MOD_INFO` string built at
//! compile time:
//!
//! ```text
//! \x01NDML-MODINFO\x02 <package> \x1f <version> \x1f <description> \x1f <default config TOML> \x03
//! ```
//!
//! Tools (the `ndml` manager) find it by scanning the file's bytes, so they can
//! show mods and write default configs **without loading or running the DLL**,
//! and without any unsafe code.

#![forbid(unsafe_code)]

/// Start marker of the embedded record.
pub const START: &str = "\u{1}NDML-MODINFO\u{2}";
/// Field separator.
pub const SEP: char = '\u{1f}';
/// End marker.
pub const END: char = '\u{3}';

/// A mod's metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModMeta {
    /// Cargo package name.
    pub package: String,
    pub version: String,
    pub description: String,
    /// Shipped default config (TOML, may contain comments); empty if none.
    pub default_config: String,
}

/// Find and decode the record in a DLL's bytes. If several candidates exist,
/// the last well-formed one wins.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<ModMeta> {
    let start = START.as_bytes();
    let mut found = None;
    let mut from = 0;
    while let Some(pos) = find(&bytes[from..], start) {
        let body_start = from + pos + start.len();
        from = body_start;
        let Some(len) = bytes[body_start..].iter().position(|&b| b == END as u8) else {
            break;
        };
        let Ok(body) = std::str::from_utf8(&bytes[body_start..body_start + len]) else {
            continue;
        };
        let mut fields = body.splitn(4, SEP);
        if let (Some(p), Some(v), Some(d), Some(c)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        {
            found = Some(ModMeta {
                package: p.to_owned(),
                version: v.to_owned(),
                description: d.to_owned(),
                default_config: c.to_owned(),
            });
        }
    }
    found
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let blob =
            format!("junk{START}magnet-range\u{1f}0.1.0\u{1f}Scales\u{1f}a = 1.0\n\u{3}junk");
        let meta = parse(blob.as_bytes()).unwrap();
        assert_eq!(meta.package, "magnet-range");
        assert_eq!(meta.version, "0.1.0");
        assert_eq!(meta.default_config, "a = 1.0\n");
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(parse(format!("{START}only\u{1f}two").as_bytes()), None);
        assert_eq!(parse(b"nothing here"), None);
    }
}
