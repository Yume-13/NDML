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

//! Editing a mod's flat TOML config (numbers, booleans, strings) while keeping
//! the file's comments and layout intact (`toml_edit`).
//!
//! A string becomes a **choice** when a comment line directly above it lists
//! the allowed values: `# options: low, high` (see `docs/mod-api.md`). Choices
//! cycle with ←/→ and typed input must be one of them. The comment is looked up
//! in the config file first, then in the mod's shipped defaults, so configs
//! written by an older version of a mod still get its choices.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Value};

/// A loaded config file, edited in place and saved on every change.
#[derive(Debug, Clone)]
pub struct ConfigDoc {
    pub path: PathBuf,
    doc: DocumentMut,
    /// The mod's shipped defaults, for choice declarations.
    defaults: Option<DocumentMut>,
}

/// A displayable field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub key: String,
    pub value: String,
    pub kind: &'static str,
    /// Value as a number, for gauges.
    pub number: Option<f64>,
    /// Allowed values for a choice field (empty otherwise).
    pub options: Vec<String>,
}

/// Comment prefix that declares a choice field's values.
const OPTIONS_TAG: &str = "options:";

/// Values from an `# options: a, b | c` line in a key's leading comments.
fn parse_options(prefix: &str) -> Vec<String> {
    prefix
        .lines()
        .filter_map(|l| l.trim().strip_prefix('#'))
        .filter_map(|l| l.trim().strip_prefix(OPTIONS_TAG))
        .next_back()
        .map(|list| {
            list.split([',', '|'])
                .map(|o| o.trim().trim_matches('"').to_owned())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Step for ←/→ on a float: one tenth of its order of magnitude (3.0 → 0.1,
/// 0.8 → 0.01, 0.01 → 0.001), at least 0.001 (the display precision).
fn float_step(v: f64) -> f64 {
    if !v.is_finite() || v.abs() < 1e-9 {
        return 0.1;
    }
    10f64.powf(v.abs().log10().floor() - 1.0).max(0.001)
}

impl ConfigDoc {
    /// Load `path`; if it doesn't exist, start from `default_text` (and save it).
    pub fn load(path: &Path, default_text: &str) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if default_text.trim().is_empty() {
                    return Err("this mod has no config".into());
                }
                default_text.to_owned()
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg = Self {
            path: path.to_path_buf(),
            doc,
            defaults: default_text.parse::<DocumentMut>().ok(),
        };
        if !path.exists() {
            cfg.save()?;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::write(&self.path, self.doc.to_string())
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Top-level keys holding plain values, in file order.
    fn keys(&self) -> Vec<String> {
        self.doc
            .iter()
            .filter(|(_, item)| item.is_value())
            .map(|(k, _)| k.to_owned())
            .collect()
    }

    /// Allowed values declared for `key` (see the module docs).
    fn options(&self, key: &str) -> Vec<String> {
        let declared = |doc: &DocumentMut| {
            doc.as_table()
                .key(key)
                .and_then(|k| k.leaf_decor().prefix())
                .and_then(|p| p.as_str())
                .map(parse_options)
                .unwrap_or_default()
        };
        let own = declared(&self.doc);
        if own.is_empty() {
            self.defaults.as_ref().map(declared).unwrap_or_default()
        } else {
            own
        }
    }

    /// Options of the field at `index` (empty if it isn't a choice).
    fn options_at(&self, index: usize) -> Vec<String> {
        self.keys()
            .into_iter()
            .nth(index)
            .map(|k| self.options(&k))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.doc
            .iter()
            .filter_map(|(k, item)| item.as_value().map(|v| (k, v)))
            .map(|(k, v)| {
                let options = match v {
                    Value::String(_) => self.options(k),
                    _ => Vec::new(),
                };
                let (value, kind, number) = match v {
                    Value::Float(f) => (format_float(*f.value()), "float", Some(*f.value())),
                    #[allow(clippy::cast_precision_loss)]
                    Value::Integer(i) => (i.value().to_string(), "int", Some(*i.value() as f64)),
                    Value::Boolean(b) => (
                        if *b.value() { "ON" } else { "OFF" }.to_owned(),
                        "bool",
                        None,
                    ),
                    Value::String(s) if !options.is_empty() => (s.value().clone(), "choice", None),
                    Value::String(s) => (format!("\"{}\"", s.value()), "text", None),
                    other => (other.to_string().trim().to_owned(), "list", None),
                };
                Field {
                    key: k.to_owned(),
                    value,
                    kind,
                    number,
                    options,
                }
            })
            .collect()
    }

    fn value_mut(&mut self, index: usize) -> Option<&mut Value> {
        let key = self.keys().into_iter().nth(index)?;
        self.doc.get_mut(&key).and_then(Item::as_value_mut)
    }

    /// Nudge a number by `steps` (floats by [`float_step`], ints by 1), toggle a
    /// bool, or cycle a choice (one option per press, either direction).
    /// Returns whether anything changed.
    pub fn adjust(&mut self, index: usize, steps: i32) -> Result<bool, String> {
        let options = self.options_at(index);
        let Some(v) = self.value_mut(index) else {
            return Ok(false);
        };
        let new = match v {
            Value::Float(f) => {
                let step = float_step(*f.value());
                let raw = f.value() + step * f64::from(steps);
                Value::from((raw / step * 10.0).round() * step / 10.0)
            }
            Value::String(s) if !options.is_empty() => {
                let cur = options.iter().position(|o| o == s.value());
                let next = match (cur, steps.signum()) {
                    (None, _) => 0,
                    (Some(i), -1) => (i + options.len() - 1) % options.len(),
                    (Some(i), _) => (i + 1) % options.len(),
                };
                Value::from(options[next].as_str())
            }
            Value::Integer(i) => Value::from(i.value() + i64::from(steps)),
            Value::Boolean(b) => Value::from(!*b.value()),
            _ => return Ok(false),
        };
        replace_keeping_decor(v, new);
        self.save()?;
        Ok(true)
    }

    /// Set a field from typed input, keeping its type.
    pub fn set_from_str(&mut self, index: usize, input: &str) -> Result<(), String> {
        let input = input.trim();
        let options = self.options_at(index);
        let v = self.value_mut(index).ok_or("no such field")?;
        let new = match v {
            Value::Float(_) => Value::from(
                input
                    .parse::<f64>()
                    .map_err(|_| format!("`{input}` is not a number"))?,
            ),
            Value::Integer(_) => Value::from(
                input
                    .parse::<i64>()
                    .map_err(|_| format!("`{input}` is not an integer"))?,
            ),
            Value::Boolean(_) => Value::from(matches!(
                input.to_ascii_lowercase().as_str(),
                "true" | "on" | "1" | "yes"
            )),
            Value::String(_) if !options.is_empty() => {
                let want = input.trim_matches('"');
                if !options.iter().any(|o| o == want) {
                    return Err(format!("choose one of: {}", options.join(", ")));
                }
                Value::from(want)
            }
            Value::String(_) => Value::from(input.trim_matches('"')),
            _ => return Err("lists can't be edited here; edit the file".into()),
        };
        replace_keeping_decor(v, new);
        self.save()
    }

    /// Replace the whole file with `default_text`.
    pub fn reset(&mut self, default_text: &str) -> Result<(), String> {
        self.doc = default_text.parse().map_err(|e| format!("defaults: {e}"))?;
        self.save()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&d).unwrap();
        d.join(format!("cfg-{}.toml", std::process::id()))
    }

    #[test]
    fn edit_cycle_keeps_comments() {
        let path = scratch();
        let _ = std::fs::remove_file(&path);
        let defaults = "# how far\nrange_multiplier = 3.0\n# toggle\nenabled = true\nwatch = []\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert!(path.exists());
        assert_eq!(doc.fields()[0].value, "3.0");
        doc.adjust(0, 3).unwrap();
        assert_eq!(doc.fields()[0].value, "3.3");
        doc.adjust(1, 1).unwrap();
        assert_eq!(doc.fields()[1].value, "OFF");
        doc.set_from_str(0, "7.25").unwrap();
        assert!(doc.set_from_str(0, "abc").is_err());
        assert!(doc.set_from_str(2, "x").is_err());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# how far\nrange_multiplier = 7.25\n"),
            "{text}"
        );
        assert!(text.contains("# toggle\nenabled = false\n"), "{text}");
        let again = ConfigDoc::load(&path, "").unwrap();
        assert_eq!(again.fields()[0].number, Some(7.25));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn choice_fields_cycle_and_validate() {
        let path = scratch().with_extension("choice.toml");
        let _ = std::fs::remove_file(&path);
        let defaults = "# preset\n# options: low, high\nmode = \"low\"\nname = \"x\"\n";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        let f = &doc.fields()[0];
        assert_eq!((f.kind, f.value.as_str()), ("choice", "low"));
        assert_eq!(f.options, ["low", "high"]);
        assert_eq!(doc.fields()[1].kind, "text");
        doc.adjust(0, 10).unwrap(); // Shift still moves one option
        assert_eq!(doc.fields()[0].value, "high");
        doc.adjust(0, 1).unwrap(); // wraps
        assert_eq!(doc.fields()[0].value, "low");
        doc.adjust(0, -1).unwrap();
        assert_eq!(doc.fields()[0].value, "high");
        assert!(doc.set_from_str(0, "nowhere").is_err());
        doc.set_from_str(0, "\"low\"").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# options: low, high\nmode = \"low\"\n"),
            "{text}"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn choices_come_from_defaults_for_old_files() {
        let path = scratch().with_extension("old.toml");
        std::fs::write(
            &path,
            "mode = \"low\"
",
        )
        .unwrap();
        let defaults = "# options: low, high
mode = \"low\"
";
        let mut doc = ConfigDoc::load(&path, defaults).unwrap();
        assert_eq!(doc.fields()[0].kind, "choice");
        doc.adjust(0, 1).unwrap();
        assert_eq!(doc.fields()[0].value, "high");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn float_steps_scale_with_magnitude() {
        assert!((float_step(3.0) - 0.1).abs() < 1e-12);
        assert!((float_step(0.8) - 0.01).abs() < 1e-12);
        assert!((float_step(0.01) - 0.001).abs() < 1e-12);
        assert!((float_step(15.0) - 1.0).abs() < 1e-12);
        assert!((float_step(0.0) - 0.1).abs() < 1e-12);
        let path = scratch().with_extension("step.toml");
        let _ = std::fs::remove_file(&path);
        let mut doc = ConfigDoc::load(&path, "scale = 0.01\n").unwrap();
        doc.adjust(0, 2).unwrap();
        assert_eq!(doc.fields()[0].value, "0.012");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn parses_options_comment() {
        assert_eq!(
            parse_options("# a\n#   options: \"x\" | y ,z\n"),
            ["x", "y", "z"]
        );
        assert!(parse_options("# nothing here\n").is_empty());
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_float(2.0), "2.0");
        assert_eq!(format_float(2.25), "2.25");
        assert_eq!(format_float(0.1 + 0.2), "0.3");
    }
}
