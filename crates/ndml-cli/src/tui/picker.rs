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

//! The checklist for a list setting whose shipped config declares its
//! options (a `multi` field): state and behaviour only, drawn by `ui`.
//!
//! Entries are the options in shipped order, under their group headings,
//! after any values of the file the options do not offer (listed first,
//! ticked and marked unknown, so they are unticked rather than silently
//! dropped). Typing filters by value, label, hint or group; the cursor moves
//! over the entries the filter keeps. Confirming hands back the ticked
//! values; the editor saves them in its own order
//! (`config_edit::list_order`).

use crate::config_edit::{Field, Rgb};

/// The group the unknown values are listed under.
pub const UNKNOWN_GROUP: &str = "Not offered by this mod";

/// One entry of the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The value saved in the file.
    pub value: String,
    /// The readable name (the value when the options give none).
    pub label: String,
    pub hint: Option<String>,
    pub group: Option<String>,
    pub colour: Option<Rgb>,
    /// Not one of the options: a value from the file (a typo, an old name).
    pub unknown: bool,
}

/// A row as shown: a group heading or an entry (index into `entries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    Group(&'a str),
    Entry(usize),
}

/// The open checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The field's key (the panel title).
    pub key: String,
    /// The field's index in the config.
    pub field: usize,
    pub entries: Vec<Entry>,
    /// Which entries are ticked (parallel to `entries`).
    pub on: Vec<bool>,
    /// The entry under the cursor (index into `entries`).
    pub cursor: usize,
    pub filter: String,
}

impl Picker {
    /// The checklist for `field` (a multi field) at `index`.
    #[must_use]
    pub fn open(field: &Field, index: usize) -> Self {
        let mut entries: Vec<Entry> = Vec::new();
        let mut on = Vec::new();
        for v in &field.values {
            if !field.offers(v) && !entries.iter().any(|e| e.value == *v) {
                entries.push(Entry {
                    value: v.clone(),
                    label: v.clone(),
                    hint: Some("unknown: untick to remove".to_owned()),
                    group: Some(UNKNOWN_GROUP.to_owned()),
                    colour: None,
                    unknown: true,
                });
                on.push(true);
            }
        }
        for (i, value) in field.options.iter().enumerate() {
            let meta = field.meta.get(i).cloned().unwrap_or_default();
            entries.push(Entry {
                value: value.clone(),
                label: meta.label.unwrap_or_else(|| value.clone()),
                hint: meta.hint,
                group: meta.group,
                colour: meta.colour,
                unknown: false,
            });
            on.push(field.values.contains(value));
        }
        Self {
            key: field.key.clone(),
            field: index,
            entries,
            on,
            cursor: 0,
            filter: String::new(),
        }
    }

    /// Whether entry `i` passes the filter.
    #[must_use]
    pub fn matches(&self, i: usize) -> bool {
        let Some(e) = self.entries.get(i) else {
            return false;
        };
        let f = self.filter.to_lowercase();
        f.is_empty()
            || [
                Some(e.value.as_str()),
                Some(e.label.as_str()),
                e.hint.as_deref(),
                e.group.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|t| t.to_lowercase().contains(&f))
    }

    /// The entries the filter keeps, in order.
    #[must_use]
    pub fn visible(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| self.matches(i))
            .collect()
    }

    /// The rows to show: each group heading before its first visible entry.
    #[must_use]
    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        let mut last: Option<Option<&str>> = None;
        for i in self.visible() {
            let group = self.entries[i].group.as_deref();
            if last != Some(group) {
                if let Some(g) = group {
                    rows.push(Row::Group(g));
                }
                last = Some(group);
            }
            rows.push(Row::Entry(i));
        }
        rows
    }

    /// Move the cursor `delta` visible entries (clamped at both ends).
    pub fn step(&mut self, delta: isize) {
        let visible = self.visible();
        let Some(pos) = visible.iter().position(|&i| i == self.cursor) else {
            self.cursor = visible.first().copied().unwrap_or(0);
            return;
        };
        let next = pos
            .saturating_add_signed(delta)
            .min(visible.len().saturating_sub(1));
        self.cursor = visible.get(next).copied().unwrap_or(self.cursor);
    }

    /// To the first (`false`) or last (`true`) visible entry.
    pub fn jump(&mut self, end: bool) {
        let visible = self.visible();
        if let Some(&i) = if end { visible.last() } else { visible.first() } {
            self.cursor = i;
        }
    }

    /// Tick or untick the entry under the cursor (if the filter keeps it).
    pub fn toggle(&mut self) {
        if self.matches(self.cursor)
            && let Some(on) = self.on.get_mut(self.cursor)
        {
            *on = !*on;
        }
    }

    /// Add a character to the filter; the cursor goes to the first match.
    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.refocus();
    }

    /// Remove the filter's last character.
    pub fn backspace(&mut self) {
        self.filter.pop();
        self.refocus();
    }

    fn refocus(&mut self) {
        if !self.matches(self.cursor) {
            self.cursor = self.visible().first().copied().unwrap_or(self.cursor);
        }
    }

    /// How many entries are ticked.
    #[must_use]
    pub fn ticked(&self) -> usize {
        self.on.iter().filter(|&&b| b).count()
    }

    /// The ticked values, in the checklist's order (unknown ones first).
    #[must_use]
    pub fn chosen(&self) -> Vec<String> {
        self.entries
            .iter()
            .zip(&self.on)
            .filter(|&(_, &on)| on)
            .map(|(e, _)| e.value.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_edit::{ConfigDoc, list_order};

    fn field(values: &str) -> Field {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "picker-{}-{}.toml",
            std::process::id(),
            values.len()
        ));
        let _ = std::fs::remove_file(&path);
        let shipped = "# Shapes.\n# options:\n#   Basic:\n#     circle = Circle (also Ring)\n#     square = Square\n#   Advanced:\n#     star = Star\n#   Bonus (extra):\n#     hexagon = Hexagon\nshapes = []\n";
        std::fs::write(&path, format!("shapes = [{values}]\n")).unwrap();
        let f = ConfigDoc::load(&path, shipped).unwrap().fields().remove(0);
        let _ = std::fs::remove_file(&path);
        f
    }

    #[test]
    fn opens_with_the_files_values_ticked_and_unknown_ones_first() {
        let f = field(r#""star", "spiral", "circle", "spiral""#);
        assert_eq!(f.kind, "multi");
        let p = Picker::open(&f, 3);
        let values: Vec<&str> = p.entries.iter().map(|e| e.value.as_str()).collect();
        assert_eq!(values, ["spiral", "circle", "square", "star", "hexagon"]);
        assert_eq!(p.on, [true, true, false, true, false]);
        assert!(p.entries[0].unknown && !p.entries[1].unknown);
        assert_eq!(p.entries[1].label, "Circle");
        assert_eq!(p.entries[1].hint.as_deref(), Some("also Ring"));
        assert_eq!(p.entries[3].group.as_deref(), Some("Advanced"));
        assert_eq!(p.ticked(), 3);
        assert_eq!(
            list_order(&f.options, &p.chosen()),
            ["circle", "star", "spiral"],
            "shipped order, then the unknown ones"
        );
    }

    #[test]
    fn the_filter_keeps_matches_and_the_cursor_on_one() {
        let mut p = Picker::open(&field(""), 0);
        assert_eq!(p.rows().len(), 7, "three headings, four entries");
        for c in "bon".chars() {
            p.type_char(c);
        }
        assert_eq!(p.visible(), [3], "by group");
        assert_eq!(p.cursor, 3);
        p.toggle();
        assert_eq!(p.chosen(), ["hexagon"]);
        p.backspace();
        p.backspace();
        p.backspace();
        for c in "RING".chars() {
            p.type_char(c);
        }
        assert_eq!(p.visible(), [0], "by hint, any case");
        assert_eq!(p.cursor, 0);
        p.filter.clear();
        p.jump(true);
        assert_eq!(p.cursor, 3);
        p.step(-10);
        assert_eq!(p.cursor, 0);
        p.step(1);
        assert_eq!(p.cursor, 1);
    }
}
