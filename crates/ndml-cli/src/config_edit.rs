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

//! Editing a mod's flat TOML config (numbers, booleans, strings).
//!
//! **Text comes from the mod as built; values come from the player's file.**
//! The fields are the keys of the mod's shipped defaults (the
//! `NDML_MOD_INFO` record in its DLL), in their order; each field's help is
//! the comment above it in the shipped defaults, and a string becomes a
//! **choice** when the shipped comment lists its values (`# options:
//! player, screen_center`; choices cycle with ←/→ and typed input must be
//! one of them). The player's file supplies the values only, so text a newer
//! version of the mod ships is what the player sees. A key the player's file
//! does not set (a file from an older version, not refreshed yet) shows its
//! default, marked as such. A key found only in the player's file is shown
//! last, as no longer used, and can't be edited.
//!
//! Loading never writes. Saving a change writes the file through
//! `ndml_sdk::config` (the one implementation the mods use too): the shipped
//! layout with the player's values, missing keys added at their default,
//! unused keys dropped, the old file kept once as `.bak` when its layout
//! changes, through a temporary file and a rename. Without shipped defaults
//! the player's file is shown and saved as it is.

use std::path::{Path, PathBuf};

use ndml_sdk::config::{
    read_text, refresh, refuse_read_only, toml_error, write_atomic, write_backup,
};
use toml_edit::{DocumentMut, Item, Value};

/// What [`ConfigDoc::adjust`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adjusted {
    /// Not a number, bool or choice: type the value instead.
    NotAdjustable,
    /// Saved; the refresh notice of an outdated file, if it was.
    Saved(Option<String>),
}

/// A loaded config file, edited in place and saved on every change.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    pub path: PathBuf,
    /// What is shown: the file as last read or written (the defaults while
    /// there is none). Saves re-read the file; this is never written back.
    doc: DocumentMut,
    /// The mod's shipped defaults (text and parsed), when it ships any.
    shipped: Option<(String, DocumentMut)>,
    /// The file as last read or written (modified time and length; `None`
    /// while there is none), to see a change made on disk since (a mod
    /// saving a choice from inside the game).
    seen: Option<(std::time::SystemTime, u64)>,
    /// What the launcher draws from `doc` and the file, worked out once and
    /// kept until either changes (a save, a reset, a re-read after the
    /// file's stamp changed): it is drawn 15 times a second.
    shown: Shown,
}

/// [`ConfigDoc`]'s cached views; an empty cell is worked out on first use.
#[derive(Debug, Clone, Default)]
struct Shown {
    fields: std::cell::OnceCell<Vec<Field>>,
    intro: std::cell::OnceCell<Vec<String>>,
    note: std::cell::OnceCell<Option<&'static str>>,
}

/// The file's modified time and length, if it exists.
fn stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// A displayable field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub key: String,
    pub value: String,
    pub kind: &'static str,
    /// Value as a number, for gauges.
    pub number: Option<f64>,
    /// Allowed values of a choice or multi field (empty otherwise).
    pub options: Vec<String>,
    /// Label, hint, group and colour of each option (parallel to `options`).
    pub meta: Vec<OptionMeta>,
    /// Any colour typed as `#rrggbb` is a valid choice too.
    pub any_colour: bool,
    /// The chosen values of a multi field, in file order.
    pub values: Vec<String>,
    /// The player's file does not set it: `value` is the mod's default.
    pub unset: bool,
    /// The mod's shipped default, when the player's value differs from it.
    pub shipped: Option<String>,
    /// What the setting does, from the shipped comment above it (the
    /// nearest one above for a key in a group under one comment), as one
    /// line.
    pub help: String,
    /// The same help as the comment's lines (for reading it in full).
    pub help_lines: Vec<String>,
    /// Only in the player's file: the mod no longer has this setting.
    pub unused: bool,
}

impl Field {
    /// The readable name of a value: its label, or the value itself.
    #[must_use]
    pub fn label_of<'a>(&'a self, value: &'a str) -> &'a str {
        self.options
            .iter()
            .position(|o| o == value)
            .and_then(|i| self.meta.get(i))
            .and_then(|m| m.label.as_deref())
            .unwrap_or(value)
    }

    /// Whether `value` is one of the options.
    #[must_use]
    pub fn offers(&self, value: &str) -> bool {
        self.options.iter().any(|o| o == value)
    }

    /// The colour of the current value: the one listed with it, or the value
    /// itself when it is a `#rrggbb` colour a choice of colours accepts.
    #[must_use]
    pub fn colour(&self) -> Option<Rgb> {
        self.options
            .iter()
            .position(|o| *o == self.value)
            .and_then(|i| self.meta.get(i))
            .and_then(|m| m.colour)
            .or_else(|| parse_hex_colour(&self.value).filter(|_| self.any_colour))
    }
}

/// A colour: red, green, blue.
pub type Rgb = [u8; 3];

/// What an `# options:` entry shows besides its value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptionMeta {
    /// The readable name (`= Label`), if given.
    pub label: Option<String>,
    /// The short hint (`(hint)` after the label), if given.
    pub hint: Option<String>,
    /// The group heading above the entry (block form), if any.
    pub group: Option<String>,
    /// The colour shown beside the value (`name #rrggbb`), if any.
    pub colour: Option<Rgb>,
}

/// A key's `# options:`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The values, in order.
    pub names: Vec<String>,
    /// Label, hint, group and colour of each value (parallel to `names`).
    pub meta: Vec<OptionMeta>,
    /// The entry [`ANY_COLOUR`]: any `#rrggbb` is a valid value too.
    pub any_colour: bool,
}

/// Comment prefix that declares a field's values.
const OPTIONS_TAG: &str = "options:";

/// The `# options:` entry that stands for any colour typed as `#rrggbb`.
pub const ANY_COLOUR: &str = "#rrggbb";

/// What a multi field says to arrows or typing.
pub const CHOOSE_FROM_LIST: &str = "Press Enter to choose from the list.";

/// `#rrggbb` (either case) as a colour.
#[must_use]
pub fn parse_hex_colour(text: &str) -> Option<Rgb> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// `# options:` with nothing after it: the start of a block.
fn is_block_start(line: &str) -> bool {
    line.trim()
        .strip_prefix('#')
        .is_some_and(|b| b.trim().eq_ignore_ascii_case(OPTIONS_TAG))
}

/// A line inside a block: `#`, at least two spaces, then text.
fn is_block_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix('#')
        .is_some_and(|b| b.starts_with("  ") && !b.trim().is_empty())
}

/// `# options: a, b` (one line, with entries).
fn is_options_line(line: &str) -> bool {
    line.trim()
        .strip_prefix('#')
        .map(str::trim)
        .and_then(|b| {
            b.get(..OPTIONS_TAG.len())
                .filter(|p| p.eq_ignore_ascii_case(OPTIONS_TAG))
        })
        .is_some()
        && !is_block_start(line)
}

/// Where the `# options:` closest to the key sits among a key's comment
/// lines: one line, or a block's first line and its entries.
fn options_span(lines: &[&str]) -> Option<std::ops::Range<usize>> {
    let i = lines
        .iter()
        .rposition(|l| is_block_start(l) || is_options_line(l))?;
    if is_block_start(lines[i]) {
        let n = lines[i + 1..]
            .iter()
            .take_while(|l| is_block_line(l))
            .count();
        Some(i..i + 1 + n)
    } else {
        Some(i..i + 1)
    }
}

/// One entry, `value [#rrggbb] [= Label (hint)]`, or [`ANY_COLOUR`].
fn add_entry(opts: &mut Options, entry: &str, group: Option<&str>) {
    let (head, label) = match entry.split_once('=') {
        Some((h, l)) => (h.trim(), Some(l.trim()).filter(|l| !l.is_empty())),
        None => (entry.trim(), None),
    };
    let head = head.trim_matches('"').trim();
    if head.is_empty() {
        return;
    }
    if head.eq_ignore_ascii_case(ANY_COLOUR) && label.is_none() {
        opts.any_colour = true;
        return;
    }
    let (name, colour) = match head.rsplit_once(char::is_whitespace) {
        Some((name, c)) if !name.trim().is_empty() && parse_hex_colour(c).is_some() => {
            (name.trim().trim_matches('"'), parse_hex_colour(c))
        }
        _ => (head, parse_hex_colour(head)),
    };
    let (label, hint) = match label {
        Some(l) => match l.strip_suffix(')').and_then(|s| s.rsplit_once('(')) {
            Some((text, hint)) if !text.trim().is_empty() => (
                Some(text.trim().to_owned()),
                Some(hint.trim().to_owned()).filter(|h| !h.is_empty()),
            ),
            _ => (Some(l.to_owned()), None),
        },
        None => (None, None),
    };
    opts.names.push(name.to_owned());
    opts.meta.push(OptionMeta {
        label,
        hint,
        group: group.map(str::to_owned),
        colour,
    });
}

/// The `# options:` in a key's leading comments (the one closest to the
/// key): `# options: a, b | "c"` on one line, or `# options:` alone and
/// then one entry per line (`#` and at least two spaces), where a line
/// ending in `:` (without `=`) is a group heading. Every entry is
/// `value [#rrggbb] [= Label (hint)]`; the entry `#rrggbb` means any colour.
#[must_use]
pub fn parse_options(prefix: &str) -> Options {
    let prefix = prefix.replace("\r\n", "\n");
    let lines: Vec<&str> = prefix.lines().collect();
    let Some(span) = options_span(&lines) else {
        return Options::default();
    };
    let mut opts = Options::default();
    if span.len() == 1 && !is_block_start(lines[span.start]) {
        let body = lines[span.start].trim().trim_start_matches('#').trim();
        for entry in body[OPTIONS_TAG.len()..].split([',', '|']) {
            add_entry(&mut opts, entry, None);
        }
        return opts;
    }
    let mut group: Option<String> = None;
    for l in &lines[span.start + 1..span.end] {
        let text = l.trim_start().trim_start_matches('#').trim();
        if let Some(heading) = text.strip_suffix(':').filter(|_| !text.contains('=')) {
            group = Some(heading.trim().to_owned());
        } else {
            add_entry(&mut opts, text, group.as_deref());
        }
    }
    opts
}

/// The help in a key's leading comments, one entry per comment line: the
/// last paragraph (a file header or section heading above it is not part of
/// it), without the `# options:` line or block and section rules.
fn parse_help_lines(prefix: &str) -> Vec<String> {
    let prefix = prefix.replace("\r\n", "\n");
    let para = prefix
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .last()
        .unwrap_or_default();
    let lines: Vec<&str> = para.lines().collect();
    let skip = options_span(&lines).unwrap_or(0..0);
    lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .filter_map(|(_, l)| l.trim().strip_prefix('#'))
        .map(|l| l.strip_prefix(' ').unwrap_or(l).trim_end())
        .filter(|l| !l.trim_start().starts_with("----"))
        .map(str::to_owned)
        .collect()
}

/// Help lines as one line.
fn join_help(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`parse_help_lines`] as one line.
#[cfg(test)]
fn parse_help(prefix: &str) -> String {
    join_help(&parse_help_lines(prefix))
}

/// The order a multi field is saved in: the options in their order, then
/// the chosen values they do not offer, in the order given; each once.
#[must_use]
pub fn list_order(options: &[String], chosen: &[String]) -> Vec<String> {
    let mut out: Vec<String> = options
        .iter()
        .filter(|o| chosen.contains(o))
        .cloned()
        .collect();
    for c in chosen {
        if !out.contains(c) {
            out.push(c.clone());
        }
    }
    out
}

/// Step for ←/→ on a float: one tenth of its order of magnitude (3.0 → 0.1,
/// 0.8 → 0.01, 0.01 → 0.001), at least 0.001 (the display precision).
fn float_step(v: f64) -> f64 {
    if !v.is_finite() || v.abs() < 1e-9 {
        return 0.1;
    }
    10f64.powf(v.abs().log10().floor() - 1.0).max(0.001)
}

fn leading_comments(doc: &DocumentMut, key: &str) -> String {
    doc.as_table()
        .key(key)
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_owned()
}

impl ConfigDoc {
    /// Load `path` against the mod's shipped `default_text`. A missing file
    /// shows the defaults; nothing is written until a value changes.
    pub fn load(path: &Path, default_text: &str) -> Result<Self, String> {
        let on_disk = read_text(path)?;
        if on_disk.is_none() && default_text.trim().is_empty() {
            return Err("this mod has no config".into());
        }
        let text = on_disk.as_deref().unwrap_or(default_text);
        let doc = parse_doc(path, text)?;
        let shipped = default_text
            .parse::<DocumentMut>()
            .ok()
            .filter(|d| !d.as_table().is_empty())
            .map(|d| (default_text.to_owned(), d));
        Ok(Self {
            path: path.to_path_buf(),
            doc,
            shipped,
            seen: stamp(path),
            shown: Shown::default(),
        })
    }

    /// Read the file again if it changed on disk since it was last read or
    /// written here (the game saved a choice while the editor was open), so
    /// the editor shows what the file holds. `Ok(true)` when it was re-read;
    /// a file that no longer parses is reported and the shown values kept.
    pub fn reload_if_changed(&mut self) -> Result<bool, String> {
        let now = stamp(&self.path);
        if now == self.seen {
            return Ok(false);
        }
        self.seen = now;
        // The file changed: what is drawn from it is worked out again, even
        // when it no longer parses (the values shown are kept then).
        self.shown = Shown::default();
        self.doc = match read_text(&self.path)? {
            Some(t) => parse_doc(&self.path, &t)?,
            None => match &self.shipped {
                Some((_, d)) => d.clone(),
                None => DocumentMut::new(),
            },
        };
        Ok(true)
    }

    fn defaults(&self) -> Option<&DocumentMut> {
        self.shipped.as_ref().map(|(_, d)| d)
    }

    /// Save `new` for `key` as a **merge on write**: the file is read again
    /// from disk (a mod may have saved its own choices from inside the game
    /// since it was loaded here), only `key` changes in it, then it goes through the
    /// shared refresh when the mod ships defaults (complete and current) and
    /// is written atomically, keeping a `.bak` of a file whose layout was
    /// older. Returns the refresh notice of the file as it was, if outdated.
    fn save_key(&mut self, key: &str, new: Value) -> Result<Option<String>, String> {
        let shown =
            |e: &dyn std::fmt::Display| format!("unable to write {} ({e})", self.path.display());
        let on_disk = read_text(&self.path)?;
        let mut doc: DocumentMut = match (&on_disk, &self.shipped) {
            (Some(t), _) => parse_doc(&self.path, t)?,
            (None, Some((_, d))) => d.clone(),
            (None, None) => DocumentMut::new(),
        };
        if let Some(slot) = doc.get_mut(key).and_then(Item::as_value_mut) {
            replace_keeping_decor(slot, new);
        } else {
            doc.insert(key, Item::Value(new));
        }
        let edited = doc.to_string();
        let (text, notice) = match &self.shipped {
            Some((shipped, _)) => {
                let text = refresh(&edited, shipped)?.text;
                let notice = on_disk
                    .as_deref()
                    .and_then(|old| refresh(old, shipped).ok())
                    .and_then(|r| r.notice());
                (text, notice)
            }
            None => (edited, None),
        };
        refuse_read_only(&self.path)?;
        if let (Some(old), Some(_)) = (&on_disk, &notice) {
            write_backup(&self.path, old)?;
        }
        write_atomic(&self.path, &text).map_err(|e| shown(&e))?;
        self.doc = parse_doc(&self.path, &text)?;
        self.seen = stamp(&self.path);
        self.shown = Shown::default();
        Ok(notice)
    }

    /// The editable keys: the shipped defaults' plain values in their order
    /// (unless the player's file holds something other than a plain value
    /// there), or, without defaults, the player's file's in file order.
    fn keys(&self) -> Vec<String> {
        match self.defaults() {
            Some(d) => d
                .iter()
                .filter(|(k, item)| item.is_value() && self.doc.get(k).is_none_or(Item::is_value))
                .map(|(k, _)| k.to_owned())
                .collect(),
            None => self
                .doc
                .iter()
                .filter(|(_, item)| item.is_value())
                .map(|(k, _)| k.to_owned())
                .collect(),
        }
    }

    /// Keys only the player's file has (none without shipped defaults).
    fn unused_keys(&self) -> Vec<String> {
        let Some(d) = self.defaults() else {
            return Vec::new();
        };
        self.doc
            .iter()
            .filter(|(k, _)| d.get(k).is_none())
            .map(|(k, _)| k.to_owned())
            .collect()
    }

    /// The value shown for `key`: the player's, else the shipped default.
    fn current(&self, key: &str) -> Option<&Value> {
        self.doc
            .get(key)
            .and_then(Item::as_value)
            .or_else(|| self.shipped_value(key))
    }

    fn shipped_value(&self, key: &str) -> Option<&Value> {
        self.defaults()
            .and_then(|d| d.get(key))
            .and_then(Item::as_value)
    }

    /// The source of a key's text: the shipped defaults, or the player's
    /// file for a mod that ships none.
    fn text_doc(&self) -> &DocumentMut {
        self.defaults().unwrap_or(&self.doc)
    }

    /// Allowed values declared for `key` (see [`parse_options`]).
    fn options(&self, key: &str) -> Options {
        parse_options(&leading_comments(self.text_doc(), key))
    }

    /// Help for `key` as comment lines: its comment, or the nearest one
    /// above it.
    fn help_lines(&self, key: &str) -> Vec<String> {
        let doc = self.text_doc();
        let mut last = Vec::new();
        for (k, _) in doc.iter() {
            let own = parse_help_lines(&leading_comments(doc, k));
            if own.iter().any(|l| !l.trim().is_empty()) {
                last = own;
            }
            if k == key {
                return last;
            }
        }
        Vec::new()
    }

    /// The introduction of the config: the comment paragraphs above its
    /// first key that are not that key's own help (the mod's own
    /// description, the file header), one entry per line, `""` between
    /// paragraphs.
    #[must_use]
    pub fn intro(&self) -> Vec<String> {
        self.shown
            .intro
            .get_or_init(|| self.work_out_intro())
            .clone()
    }

    fn work_out_intro(&self) -> Vec<String> {
        let doc = self.text_doc();
        let Some((first, _)) = doc.iter().next() else {
            return Vec::new();
        };
        let prefix = leading_comments(doc, first).replace("\r\n", "\n");
        let paras: Vec<&str> = prefix
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let mut out: Vec<String> = Vec::new();
        for p in paras.iter().take(paras.len().saturating_sub(1)) {
            if !out.is_empty() {
                out.push(String::new());
            }
            for l in p.lines().filter_map(|l| l.trim().strip_prefix('#')) {
                let l = l.strip_prefix(' ').unwrap_or(l).trim_end();
                if !l.trim_start().starts_with("----") {
                    out.push(l.to_owned());
                }
            }
        }
        while out.last().is_some_and(|l| l.trim().is_empty()) {
            out.pop();
        }
        out
    }

    /// What the file on disk is, when it is not simply current: the shipped
    /// defaults shown because there is none yet, or a file from an older
    /// version of the mod (refreshed on the next save).
    #[must_use]
    pub fn note(&self) -> Option<&'static str> {
        *self.shown.note.get_or_init(|| self.work_out_note())
    }

    fn work_out_note(&self) -> Option<&'static str> {
        let (shipped, _) = self.shipped.as_ref()?;
        match read_text(&self.path) {
            Ok(None) => Some("shipped defaults, saved on first change"),
            Ok(Some(t)) if refresh(&t, shipped).is_ok_and(|r| r.changed) => {
                Some("from an older version of the mod")
            }
            _ => None,
        }
    }

    /// Options of the field at `index`.
    fn options_at(&self, index: usize) -> Options {
        self.keys()
            .into_iter()
            .nth(index)
            .map_or_default(|k| self.options(&k))
    }

    /// Every setting as shown, in the shipped order, then the ones only the
    /// player's file has.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.shown
            .fields
            .get_or_init(|| self.work_out_fields())
            .clone()
    }

    fn work_out_fields(&self) -> Vec<Field> {
        let mut fields: Vec<Field> = self
            .keys()
            .into_iter()
            .filter_map(|k| {
                let v = self.current(&k)?;
                let opts = self.options(&k);
                let kind_of = |v: &Value| match v {
                    Value::String(_) if !opts.names.is_empty() || opts.any_colour => Some("choice"),
                    _ if !opts.names.is_empty() && string_list(v).is_some() => Some("multi"),
                    _ => None,
                };
                let special = kind_of(v);
                let (value, kind, number) = show(v, special);
                let unset = self.doc.get(&k).is_none();
                let shipped = if unset {
                    None
                } else {
                    self.shipped_value(&k)
                        .map(|d| show(d, kind_of(d)).0)
                        .filter(|d| *d != value)
                };
                let opts = if special.is_some() {
                    opts
                } else {
                    Options::default()
                };
                let help_lines = self.help_lines(&k);
                Some(Field {
                    help: join_help(&help_lines),
                    help_lines,
                    values: string_list(v)
                        .filter(|_| kind == "multi")
                        .unwrap_or_default(),
                    key: k,
                    value,
                    kind,
                    number,
                    options: opts.names,
                    meta: opts.meta,
                    any_colour: opts.any_colour,
                    unset,
                    shipped,
                    unused: false,
                })
            })
            .collect();
        fields.extend(self.unused_keys().into_iter().map(|k| {
            let value = self
                .doc
                .get(&k)
                .map_or_default(|i| i.to_string().trim().to_owned());
            let help = "No longer used by this mod; dropped on the next save.";
            Field {
                key: k,
                value,
                kind: "unused",
                number: None,
                options: Vec::new(),
                meta: Vec::new(),
                any_colour: false,
                values: Vec::new(),
                unset: false,
                shipped: None,
                help: help.into(),
                help_lines: vec![help.into()],
                unused: true,
            }
        }));
        fields
    }

    /// Save a multi field's chosen values: the options in their shipped
    /// order, then values the options do not offer (kept from the file, in
    /// the order given), each once. Nothing is written when the set is the
    /// file's. Returns the refresh notice of an outdated file.
    pub fn set_list(&mut self, index: usize, chosen: &[String]) -> Result<Option<String>, String> {
        let field = self
            .fields()
            .into_iter()
            .nth(index)
            .ok_or("no such field")?;
        if field.kind != "multi" {
            return Err("not a list to choose from".into());
        }
        let ordered = list_order(&field.options, chosen);
        if ordered == list_order(&field.options, &field.values) {
            return Ok(None);
        }
        let mut array = toml_edit::Array::new();
        for v in &ordered {
            array.push(v.as_str());
        }
        self.save_key(&field.key, Value::Array(array))
    }

    /// The key of the field at `index` and its current value.
    fn field_value(&self, index: usize) -> Result<(String, Value), String> {
        let keys = self.keys();
        let Some(key) = keys.into_iter().nth(index) else {
            return Err("this setting is no longer used by the mod".into());
        };
        let v = self.current(&key).ok_or("no such field")?.clone();
        Ok((key, v))
    }

    /// Nudge a number by `steps` (floats by [`float_step`], ints by 1), toggle a
    /// bool, or cycle a choice (one option per press, either direction).
    /// Returns whether anything changed, and the refresh notice of an
    /// outdated file.
    pub fn adjust(&mut self, index: usize, steps: i32) -> Result<Adjusted, String> {
        let options = self.options_at(index).names;
        let (key, v) = self.field_value(index)?;
        let new = match &v {
            Value::Float(f) => {
                let step = float_step(*f.value());
                let raw = f.value() + step * f64::from(steps);
                Value::from((raw / step * 10.0).round() * step / 10.0)
            }
            Value::String(s) if !options.is_empty() => {
                let cur = options.iter().position(|o| o == s.value());
                // A value not in the list sits past either end of it.
                let next = match (cur, steps.signum()) {
                    (None, -1) => options.len() - 1,
                    (None, _) => 0,
                    (Some(i), -1) => (i + options.len() - 1) % options.len(),
                    (Some(i), _) => (i + 1) % options.len(),
                };
                Value::from(options[next].as_str())
            }
            Value::Integer(i) => Value::from(i.value().saturating_add(i64::from(steps))),
            Value::Boolean(b) => Value::from(!*b.value()),
            _ => return Ok(Adjusted::NotAdjustable),
        };
        self.save_key(&key, new).map(Adjusted::Saved)
    }

    /// Set a field from user input, keeping its type. Returns the refresh
    /// notice of an outdated file.
    pub fn set_from_str(&mut self, index: usize, input: &str) -> Result<Option<String>, String> {
        let input = input.trim();
        let opts = self.options_at(index);
        let options = &opts.names;
        let (key, v) = self.field_value(index)?;
        let new = match v {
            Value::Float(_) => Value::from(
                input
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| format!("`{input}` is not a number"))?,
            ),
            Value::Integer(_) => Value::from(
                input
                    .parse::<i64>()
                    .map_err(|_| format!("`{input}` is not a whole number"))?,
            ),
            // Anything else is refused: a typo must not save `false`.
            Value::Boolean(_) => Value::from(match input.to_ascii_lowercase().as_str() {
                "true" | "on" | "1" | "yes" => true,
                "false" | "off" | "0" | "no" => false,
                _ => return Err(format!("`{input}` is not true or false")),
            }),
            Value::String(_) if !options.is_empty() || opts.any_colour => {
                let want = input.trim_matches('"');
                if options.iter().any(|o| o == want) {
                    Value::from(want)
                } else if let Some([r, g, b]) = parse_hex_colour(want).filter(|_| opts.any_colour) {
                    Value::from(format!("#{r:02x}{g:02x}{b:02x}"))
                } else {
                    let mut list = options.clone();
                    if opts.any_colour {
                        list.push(ANY_COLOUR.to_owned());
                    }
                    return Err(format!("choose one of: {}", list.join(", ")));
                }
            }
            Value::String(_) => Value::from(input.trim_matches('"')),
            _ if !options.is_empty() && string_list(&v).is_some() => {
                return Err(CHOOSE_FROM_LIST.into());
            }
            _ => return Err("lists can't be edited here; edit the file".into()),
        };
        self.save_key(&key, new)
    }

    /// Replace the whole file with the shipped defaults (the old one kept as
    /// `.bak`).
    pub fn reset(&mut self) -> Result<(), String> {
        let Some((shipped, parsed)) = self.shipped.clone() else {
            return Err("this mod ships no defaults".into());
        };
        refuse_read_only(&self.path)?;
        if let Some(old) = read_text(&self.path)?.filter(|o| *o != shipped) {
            write_backup(&self.path, &old)?;
        }
        write_atomic(&self.path, &shipped)
            .map_err(|e| format!("unable to write {} ({e})", self.path.display()))?;
        self.doc = parsed;
        self.seen = stamp(&self.path);
        self.shown = Shown::default();
        Ok(())
    }
}

/// The strings of an array whose every element is a string.
fn string_list(v: &Value) -> Option<Vec<String>> {
    v.as_array()?
        .iter()
        .map(|e| e.as_str().map(str::to_owned))
        .collect()
}

/// A value as shown: (text, kind, number for gauges). `special`: the key
/// declares options, so a string is a `choice` and a list of strings a
/// `multi`.
fn show(v: &Value, special: Option<&'static str>) -> (String, &'static str, Option<f64>) {
    let choice = special == Some("choice");
    if special == Some("multi")
        && let Some(list) = string_list(v)
    {
        return (list.join(", "), "multi", None);
    }
    match v {
        Value::Float(f) => (format_float(*f.value()), "float", Some(*f.value())),
        #[allow(clippy::cast_precision_loss)] // only a gauge position; config integers are small
        Value::Integer(i) => (i.value().to_string(), "int", Some(*i.value() as f64)),
        Value::Boolean(b) => (
            if *b.value() { "ON" } else { "OFF" }.to_owned(),
            "bool",
            None,
        ),
        Value::String(s) if choice => (s.value().clone(), "choice", None),
        Value::String(s) => (format!("\"{}\"", s.value()), "text", None),
        other => (other.to_string().trim().to_owned(), "list", None),
    }
}

/// Swap a value but keep the whitespace/comment decoration around it.
fn replace_keeping_decor(slot: &mut Value, mut new: Value) {
    *new.decor_mut() = slot.decor().clone();
    *slot = new;
}

fn format_float(f: f64) -> String {
    let s = format!("{f:.3}");
    let s = s.trim_end_matches('0');
    if s.ends_with('.') {
        format!("{s}0")
    } else {
        s.to_owned()
    }
}

/// `text` with the full `path` in it replaced by the file's name (a status
/// line has no room for the whole path).
#[must_use]
pub fn by_file_name(text: &str, path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    text.replace(&path.display().to_string(), &name)
}

/// `text` (the file at `path`) parsed for editing; a malformed file is
/// reported in one line with its line number.
fn parse_doc(path: &Path, text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| {
        format!(
            "{}: {}",
            path.display(),
            toml_error(text, e.span(), e.message())
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndml_sdk::config::backup_path;

    /// Typed values that are not of the setting's type are refused and the
    /// file keeps its value: a typo in a bool never saves `false`; NaN and
    /// infinity are not numbers; steps stop at the integer limits; ← from a
    /// value the options do not offer goes to the last option.
    #[test]
    fn typed_values_must_be_of_the_settings_type() {
        let path = scratch().with_extension("typed.toml");
        clean(&path);
        let defaults = "# on\nflag = true\n# far\nrange = 3.0\n# many\ncount = 9223372036854775806\n# options: a, b, c\nmode = \"a\"\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        for typo in ["ture", "of", "2", "enabled", ""] {
            let e = doc.set_from_str(0, typo).unwrap_err();
            assert_eq!(e, format!("`{typo}` is not true or false"));
        }
        assert!(!path.exists(), "nothing saved");
        assert_eq!(doc.fields()[0].value, "ON");
        for (input, want) in [("off", "OFF"), ("Yes", "ON"), ("0", "OFF"), ("TRUE", "ON")] {
            doc.set_from_str(0, input).unwrap();
            assert_eq!(doc.fields()[0].value, want, "{input}");
        }
        for bad in ["nan", "inf", "-infinity", "NaN"] {
            assert_eq!(
                doc.set_from_str(1, bad).unwrap_err(),
                format!("`{bad}` is not a number")
            );
        }
        assert_eq!(
            doc.set_from_str(2, "1.5").unwrap_err(),
            "`1.5` is not a whole number"
        );
        doc.adjust(2, 10).unwrap();
        assert_eq!(doc.fields()[2].value, i64::MAX.to_string());
        std::fs::write(
            &path,
            std::fs::read_to_string(&path)
                .unwrap()
                .replace("mode = \"a\"", "mode = \"zz\""),
        )
        .unwrap();
        doc.reload_if_changed().unwrap();
        doc.adjust(3, -1).unwrap();
        assert_eq!(doc.fields()[3].value, "c", "← from outside the list: last");
        clean(&path);
    }

    /// A change the game makes on disk while the editor is open is shown:
    /// the editor reads the file again when it changed, and only then.
    #[test]
    fn the_editor_reads_a_file_changed_on_disk() {
        let path = scratch().with_extension("reread.toml");
        clean(&path);
        let defaults = "# theme\ntheme = \"plain\"\n";
        std::fs::write(&path, defaults).unwrap();
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert!(!doc.reload_if_changed().unwrap(), "unchanged: not re-read");
        doc.set_from_str(0, "bright").unwrap();
        assert!(!doc.reload_if_changed().unwrap(), "the editor's own save");
        // A different length: the change shows even within the clock's grain.
        std::fs::write(&path, "# theme\ntheme = \"dark\"   \n").unwrap();
        assert!(doc.reload_if_changed().unwrap());
        assert_eq!(doc.fields()[0].value, "\"dark\"");
        std::fs::write(&path, "theme = [\n").unwrap();
        let e = doc.reload_if_changed().unwrap_err();
        assert!(e.contains("not valid TOML (line 1: "), "{e}");
        assert!(!e.contains('\n'), "{e}");
        assert_eq!(doc.fields()[0].value, "\"dark\"", "kept on a bad file");
        std::fs::remove_file(&path).unwrap();
        assert!(doc.reload_if_changed().unwrap());
        assert_eq!(doc.fields()[0].value, "\"plain\"", "gone: the defaults");
        clean(&path);
    }

    /// What the launcher draws is worked out once and kept, and follows
    /// every change: a save, a reset, the file changed on disk.
    #[test]
    fn the_cached_views_follow_every_change() {
        let path = scratch().with_extension("cache.toml");
        clean(&path);
        let defaults = "# The mod.\n\n# theme\ntheme = \"plain\"\n# how far\nrange = 1.0\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert_eq!(doc.note(), Some("shipped defaults, saved on first change"));
        assert_eq!(doc.intro(), ["The mod."]);
        assert_eq!(doc.fields()[1].value, "1.0");
        assert_eq!(doc.fields(), doc.fields(), "kept");
        doc.set_from_str(1, "2.5").unwrap();
        assert_eq!(doc.fields()[1].value, "2.5", "a save");
        assert_eq!(doc.note(), None, "saved: current");
        std::fs::write(&path, "theme = \"dark\"\nrange = 4.0\n").unwrap();
        assert!(doc.reload_if_changed().unwrap());
        assert_eq!(doc.fields()[1].value, "4.0", "changed on disk");
        assert_eq!(doc.note(), Some("from an older version of the mod"));
        doc.reset().unwrap();
        assert_eq!(doc.fields()[1].value, "1.0", "a reset");
        assert_eq!(doc.note(), None);
        clean(&path);
    }

    fn scratch() -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&d).unwrap();
        d.join(format!("cfg-{}.toml", std::process::id()))
    }

    fn clean(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup_path(path));
    }

    #[test]
    fn edit_cycle_keeps_comments_and_viewing_writes_nothing() {
        let path = scratch();
        clean(&path);
        let defaults = "# how far\nrange_multiplier = 3.0\n# toggle\nenabled = true\nwatch = []\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert!(!path.exists(), "viewing writes nothing");
        assert_eq!(doc.fields()[0].value, "3.0");
        assert_eq!(doc.fields()[0].help, "how far");
        doc.adjust(0, 3).unwrap();
        assert_eq!(doc.fields()[0].value, "3.3");
        doc.adjust(1, 1).unwrap();
        assert_eq!(doc.fields()[1].value, "OFF");
        doc.set_from_str(0, "7.25").unwrap();
        assert!(doc.set_from_str(0, "abc").is_err());
        assert!(doc.set_from_str(2, "x").is_err());

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# how far\nrange_multiplier = 7.25\n# toggle\nenabled = false\nwatch = []\n"
        );
        let again = ConfigDoc::load(&path, "").unwrap();
        assert_eq!(again.fields()[0].number, Some(7.25));
        clean(&path);
    }

    #[test]
    fn choice_fields_cycle_and_validate() {
        let path = scratch().with_extension("choice.toml");
        clean(&path);
        let defaults = "# what to follow\n# options: player, screen_center\nanchor = \"player\"\nname = \"x\"\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        let f = &doc.fields()[0];
        assert_eq!((f.kind, f.value.as_str()), ("choice", "player"));
        assert_eq!(f.options, ["player", "screen_center"]);
        assert_eq!(f.help, "what to follow");
        assert_eq!(doc.fields()[1].kind, "text");
        doc.adjust(0, 10).unwrap(); // Shift still moves one option
        assert_eq!(doc.fields()[0].value, "screen_center");
        doc.adjust(0, 1).unwrap(); // wraps
        assert_eq!(doc.fields()[0].value, "player");
        doc.adjust(0, -1).unwrap();
        assert_eq!(doc.fields()[0].value, "screen_center");
        assert!(doc.set_from_str(0, "nowhere").is_err());
        doc.set_from_str(0, "\"player\"").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# options: player, screen_center\nanchor = \"player\"\n"),
            "{text}"
        );
        clean(&path);
    }

    /// The text (options, help) is the shipped one, never the player's file's.
    #[test]
    fn text_comes_from_the_shipped_defaults() {
        let path = scratch().with_extension("text.toml");
        clean(&path);
        std::fs::write(
            &path,
            "# old help\n# options: native, chosen\ntint = \"off\"\n",
        )
        .unwrap();
        let defaults =
            "# How the ship is coloured.\n# options: off, native, chosen\ntint = \"off\"\n";
        let doc = ConfigDoc::load(&path, defaults).unwrap();
        let f = &doc.fields()[0];
        assert_eq!(f.options, ["off", "native", "chosen"]);
        assert_eq!(f.help, "How the ship is coloured.");
        clean(&path);
    }

    /// A file from an older version: viewing it writes nothing; a key only
    /// the file has shows as unused; the first save writes the current
    /// layout with the player's values, keeps a `.bak`, and says what changed.
    #[test]
    fn an_old_file_is_shown_then_saved_complete() {
        let path = scratch().with_extension("new.toml");
        clean(&path);
        let player = "# my own note\npull_strength = 1.5 # I like it stronger\nold_knob = 1\n\n# trailing words\n";
        std::fs::write(&path, player).unwrap();
        let defaults = "# How strongly pickups are pulled.\npull_strength = 1.2\n\n# How long a fade takes.\nfade_seconds = 15.0\n# options: a, b\nmode = \"a\"\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), player);
        let f = doc.fields();
        let keys: Vec<&str> = f.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["pull_strength", "fade_seconds", "mode", "old_knob"]);
        assert_eq!((f[0].value.as_str(), f[0].unset), ("1.5", false));
        assert_eq!(f[0].shipped.as_deref(), Some("1.2"));
        assert_eq!(
            (f[1].value.as_str(), f[1].unset, f[1].shipped.clone()),
            ("15.0", true, None)
        );
        assert_eq!((f[2].kind, f[2].unset), ("choice", true));
        assert!(f[3].unused && f[3].kind == "unused");
        assert!(doc.adjust(3, 1).is_err(), "an unused key can't be edited");
        assert_eq!(
            doc.adjust(1, 1).unwrap(),
            Adjusted::Saved(Some(
                "config refreshed: fade_seconds added (15.0), mode added (\"a\"), old_knob dropped (was 1)"
                    .into()
            ))
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# How strongly pickups are pulled.\npull_strength = 1.5\n\n# How long a fade takes.\nfade_seconds = 16.0\n# options: a, b\nmode = \"a\"\n"
        );
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), player);
        // Now current: the next save says nothing more.
        assert_eq!(doc.set_from_str(2, "b").unwrap(), None);
        assert_eq!(doc.fields().len(), 3);
        doc.reset().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), defaults);
        clean(&path);
    }

    /// Merge on write: a value the game saved into the file after the
    /// editor loaded it (a theme chosen in game) survives an editor save.
    #[test]
    fn an_editor_save_keeps_values_saved_in_game_since() {
        let path = scratch().with_extension("merge.toml");
        clean(&path);
        let defaults =
            "# Theme.\ntheme = \"plain\"\n# Labels.\nshow_labels = false\n# Size.\nsize = 2.0\n";
        std::fs::write(&path, defaults).unwrap();
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        // The game saves its own choices meanwhile (only its keys).
        std::fs::write(
            &path,
            defaults
                .replace("\"plain\"", "\"bright\"")
                .replace("show_labels = false", "show_labels = true"),
        )
        .unwrap();
        doc.set_from_str(2, "1.5").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("theme = \"bright\"")
                && text.contains("show_labels = true")
                && text.contains("size = 1.5"),
            "{text}"
        );
        // The display follows the file as saved.
        assert_eq!(doc.fields()[0].value, "\"bright\"");
        // A malformed file at save time is left alone.
        std::fs::write(&path, "theme = [").unwrap();
        assert!(doc.set_from_str(2, "1.2").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme = [");
        clean(&path);
    }

    #[test]
    fn grouped_keys_share_the_comment_above() {
        let path = scratch().with_extension("group.toml");
        clean(&path);
        let defaults = "# Mod: about.\n#\n# Header words.\n\n# Move enemies.\nmove = true\nturn = true\n# Other.\nother = 1\n";
        let doc = ConfigDoc::load(&path, defaults).unwrap();
        let help: Vec<String> = doc.fields().into_iter().map(|f| f.help).collect();
        assert_eq!(help, ["Move enemies.", "Move enemies.", "Other."]);
    }

    #[test]
    fn without_defaults_the_file_is_shown_as_it_is() {
        let path = scratch().with_extension("nodef.toml");
        std::fs::write(&path, "b = 2\na = true\n").unwrap();
        let doc = ConfigDoc::load(&path, "").unwrap();
        let keys: Vec<String> = doc.fields().into_iter().map(|f| f.key).collect();
        assert_eq!(keys, ["b", "a"]);
        assert!(
            doc.fields()
                .iter()
                .all(|f| !f.unset && f.shipped.is_none() && !f.unused)
        );
        clean(&path);
    }

    #[test]
    fn float_steps_scale_with_magnitude() {
        assert!((float_step(3.0) - 0.1).abs() < 1e-12);
        assert!((float_step(0.8) - 0.01).abs() < 1e-12);
        assert!((float_step(0.01) - 0.001).abs() < 1e-12);
        assert!((float_step(15.0) - 1.0).abs() < 1e-12);
        assert!((float_step(0.0) - 0.1).abs() < 1e-12);
        let path = scratch().with_extension("step.toml");
        clean(&path);
        let mut doc = ConfigDoc::load(&path, "drift = 0.01\n").unwrap();
        doc.adjust(0, 2).unwrap();
        assert_eq!(doc.fields()[0].value, "0.012");
        clean(&path);
    }

    #[test]
    fn parses_options_and_help_comments() {
        assert_eq!(
            parse_options("# a\n#   options: \"x\" | y ,z\n").names,
            ["x", "y", "z"]
        );
        assert!(parse_options("# nothing here\n").names.is_empty());
        assert_eq!(
            parse_help("# File: about.\n\n# ---- Section ----\n# One.\n# Two.\n# options: a, b\n"),
            "One. Two."
        );
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_float(2.0), "2.0");
        assert_eq!(format_float(2.25), "2.25");
        assert_eq!(format_float(0.1 + 0.2), "0.3");
    }
}
