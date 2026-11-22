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

//! TOML config files for mods.
//!
//! A mod ships its defaults as a TOML file (`mods/<mod>/config.toml`, embedded
//! with `export_mod!(init, defaults = include_str!("../config.toml"))`). That
//! text is the single source of truth for the settings, their order, their
//! help comments and their default values. The player's file supplies values
//! and nothing else.
//!
//! **The value of every setting the mod has is kept; the file is kept
//! current.** When the player's file differs from the shipped one in its
//! settings or its comments, [`refresh`] rewrites it as the shipped layout
//! with the player's values carried over as written (type and formatting
//! kept), settings the file lacks added at their default, and settings the
//! mod does not have (retired, or misspelt) dropped; comments the player
//! added are replaced by the shipped ones. The old file is kept beside it as
//! `<name>.toml.bak`, the last version only (the next refresh replaces it).
//! A file whose values alone differ is left as it is, so running the
//! refresh again changes nothing. The same function serves the mod at load
//! time ([`load`]) and the `ndml` manager (install and its editor).
//!
//! Loading never fails because of the player's file: a setting the mod does
//! not know is dropped by the refresh (skipped with one warning when the file
//! can't be rewritten), a value of the wrong type falls back
//! to that setting's default alone, and a malformed file is reported and left
//! untouched (defaults used). Only broken *shipped* defaults are an error.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use toml::de::{DeTable, DeValue};

/// Parse the shipped defaults alone. Use this in a mod's unit test to prove its
/// `config.toml` matches its `Config` type.
pub fn parse_defaults<T: DeserializeOwned>(defaults: &str) -> Result<T, String> {
    toml::from_str(defaults).map_err(|e| format!("built-in defaults are invalid: {e}"))
}

/// A setting named in a [`Refresh`]: its key and its value as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
}

/// What [`refresh`] makes of a player's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refresh {
    /// The file as it should be: the shipped layout with the player's values.
    pub text: String,
    /// Whether `text` differs from the player's file (line endings aside).
    pub changed: bool,
    /// Settings the player's file lacked, at their shipped default.
    pub added: Vec<Setting>,
    /// Settings the player's file had that the mod no longer ships, left
    /// out of `text` ([`Retired::Drop`]).
    pub dropped: Vec<Setting>,
    /// The same, kept at the end of `text` under [`UNUSED_HEADING`]
    /// ([`Retired::KeepUnused`]).
    pub kept: Vec<Setting>,
}

/// What a refresh does with a setting the player's file has and the mod no
/// longer ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// Left out of the file (named with its value in the notice; the
    /// backup keeps it).
    Drop,
    /// Kept, after the shipped layout, under [`UNUSED_HEADING`].
    KeepUnused,
}

/// The policy every refresh uses.
pub const RETIRED: Retired = Retired::Drop;

/// The comment above settings kept by [`Retired::KeepUnused`].
pub const UNUSED_HEADING: &str = "# No longer used by this mod.";

impl Refresh {
    /// The player-facing notice, `None` when nothing changes:
    /// `config refreshed: pull_radius added (20), old_radius dropped (was 20)`,
    /// or `config refreshed: comments updated` when only the text changed.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        if !self.changed {
            return None;
        }
        let mut parts: Vec<String> = self
            .added
            .iter()
            .map(|s| format!("{} added ({})", s.key, one_line_value(&s.value)))
            .collect();
        parts.extend(
            self.dropped
                .iter()
                .map(|s| format!("{} dropped (was {})", s.key, one_line_value(&s.value))),
        );
        parts.extend(self.kept.iter().map(|s| format!("{} unused", s.key)));
        if parts.is_empty() {
            parts.push("comments updated".to_owned());
        }
        Some(format!("config refreshed: {}", parts.join(", ")))
    }
}

/// A TOML parse error in one line: `not valid TOML (line 2: duplicate key)`.
/// `span` is the error's byte range in `text`, `message` its own text
/// (both `toml` and `toml_edit` errors give them).
#[must_use]
pub fn toml_error(text: &str, span: Option<std::ops::Range<usize>>, message: &str) -> String {
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    match span.and_then(|s| text.get(..s.start)) {
        Some(before) => format!(
            "not valid TOML (line {}: {message})",
            before.matches('\n').count() + 1
        ),
        None => format!("not valid TOML ({message})"),
    }
}

fn de_error(text: &str, e: &toml::de::Error) -> String {
    toml_error(text, e.span(), e.message())
}

/// Refuse to write a settings file the player made read-only: the attribute
/// is their choice, so it is reported, never cleared.
pub fn refuse_read_only(path: &Path) -> Result<(), String> {
    if std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
        Err(format!(
            "{} is read-only; clear its read-only attribute to change it",
            path.display()
        ))
    } else {
        Ok(())
    }
}

/// Read a settings file as text: `Ok(None)` when there is none; a file that
/// can't be read or is not UTF-8 is an error naming it.
pub fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| format!("{} is not UTF-8 text", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("unable to read {} ({e})", path.display())),
    }
}

/// The top-level settings of a TOML text in file order: key and the byte
/// range of its value.
fn entries(text: &str) -> Result<Vec<(String, std::ops::Range<usize>)>, String> {
    let table = DeTable::parse(text).map_err(|e| de_error(text, &e))?;
    let mut out: Vec<(String, std::ops::Range<usize>, usize)> = table
        .get_ref()
        .iter()
        .map(|(k, v)| (k.get_ref().to_string(), v.span(), k.span().start))
        .collect();
    out.sort_by_key(|e| e.2);
    Ok(out.into_iter().map(|(k, v, _)| (k, v)).collect())
}

/// Whether `raw` is a value that can stand on its own after `key = `
/// (not a `[table]` section).
fn is_inline_value(raw: &str) -> bool {
    DeValue::parse(raw).is_ok()
}

/// Bring a player's settings file up to date with the shipped defaults
/// (see the module docs). `Err` when either text is not valid TOML, or the
/// shipped defaults hold a `[table]` (not supported: settings are flat).
pub fn refresh(user: &str, shipped: &str) -> Result<Refresh, String> {
    refresh_with(user, shipped, RETIRED)
}

/// [`refresh`] with the policy for retired settings given.
pub fn refresh_with(user: &str, shipped: &str, retired: Retired) -> Result<Refresh, String> {
    let layout = entries(shipped).map_err(|e| format!("built-in defaults are invalid: {e}"))?;
    if let Some((k, _)) = layout
        .iter()
        .find(|(_, r)| !is_inline_value(&shipped[r.clone()]))
    {
        return Err(format!("built-in defaults: {k} is a table"));
    }
    let theirs = entries(user)?;
    let raw_of = |key: &str| {
        theirs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, r)| user[r.clone()].to_owned())
    };
    let mut text = String::with_capacity(shipped.len() + 64);
    let mut added = Vec::new();
    // The player's `[table]` where the mod ships a plain value: not kept,
    // so named as dropped with what it held.
    let mut replaced = Vec::new();
    let mut pos = 0;
    for (key, range) in &layout {
        text.push_str(&shipped[pos..range.start]);
        match raw_of(key) {
            Some(raw) if is_inline_value(&raw) => text.push_str(&raw),
            other => {
                if let Some(raw) = other {
                    // The span covers only a `[table]`'s header: show what
                    // it held, as an inline table.
                    let value = user
                        .parse::<toml::Table>()
                        .ok()
                        .and_then(|t| t.get(key).map(ToString::to_string))
                        .unwrap_or(raw);
                    replaced.push(Setting {
                        key: key.clone(),
                        value,
                    });
                }
                let default = &shipped[range.clone()];
                text.push_str(default);
                added.push(Setting {
                    key: key.clone(),
                    value: default.to_owned(),
                });
            }
        }
        pos = range.end;
    }
    text.push_str(&shipped[pos..]);
    let retired_keys: Vec<Setting> = theirs
        .iter()
        .filter(|(k, _)| !layout.iter().any(|(s, _)| s == k))
        .map(|(k, r)| Setting {
            key: k.clone(),
            value: user[r.clone()].to_owned(),
        })
        .collect();
    let (mut dropped, kept) = match retired {
        Retired::Drop => (retired_keys, Vec::new()),
        Retired::KeepUnused => keep_unused(&mut text, retired_keys),
    };
    dropped.splice(0..0, replaced);
    let changed = text.replace("\r\n", "\n") != user.replace("\r\n", "\n");
    Ok(Refresh {
        text,
        changed,
        added,
        dropped,
        kept,
    })
}

/// [`Retired::KeepUnused`]: append the retired settings that are plain
/// values to `text` under [`UNUSED_HEADING`]. Returns (dropped, kept): a
/// retired `[table]` can't be kept this way and is dropped.
fn keep_unused(text: &mut String, retired: Vec<Setting>) -> (Vec<Setting>, Vec<Setting>) {
    let (kept, dropped): (Vec<Setting>, Vec<Setting>) =
        retired.into_iter().partition(|s| is_inline_value(&s.value));
    if !kept.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(UNUSED_HEADING);
        text.push('\n');
        for s in &kept {
            text.push_str(&s.key);
            text.push_str(" = ");
            text.push_str(&s.value);
            text.push('\n');
        }
    }
    (dropped, kept)
}

/// The backup of a settings file: `<name>.toml.bak` beside it, a name nothing
/// reads as a settings file.
#[must_use]
pub fn backup_path(path: &Path) -> PathBuf {
    with_suffix(path, "bak")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Clear the read-only attribute of one of the SDK's own files (a leftover
/// temporary file, a backup) before it is replaced. Never used on the
/// player's settings file: that attribute is theirs ([`refuse_read_only`]).
fn make_replaceable(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.permissions().readonly() => {
            let mut perms = m.permissions();
            #[allow(clippy::permissions_set_readonly_false)] // Windows: clears the attribute only
            perms.set_readonly(false);
            std::fs::set_permissions(path, perms)
        }
        _ => Ok(()),
    }
}

/// Replace `path` with `text` through a temporary file beside it and a
/// rename, so a reader never sees half a file. A read-only temporary file
/// left from an earlier write is the SDK's own and is replaced; a read-only `path`
/// is not touched (the rename fails).
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = with_suffix(path, "tmp");
    make_replaceable(&tmp)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Keep `old` as the backup of the settings file at `path`
/// ([`backup_path`]), replacing an earlier backup even if it was made
/// read-only (the backup is the SDK's own).
pub fn write_backup(path: &Path, old: &str) -> Result<(), String> {
    let backup = backup_path(path);
    make_replaceable(&backup)
        .and_then(|()| write_atomic(&backup, old))
        .map_err(|e| format!("unable to write {} ({e})", backup.display()))
}

/// What [`update_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileUpdate {
    /// No file: the shipped defaults were written.
    Created,
    /// No file, and none written (`create` was false or no defaults).
    Missing,
    /// The file is current.
    Unchanged,
    /// The file was rewritten: the notice ([`Refresh::notice`]) and the
    /// numbers of settings added and dropped (or kept as unused).
    Refreshed {
        notice: String,
        added: usize,
        dropped: usize,
    },
}

/// Bring the settings file at `path` up to date with `shipped` (see
/// [`refresh`]): a missing file is written from the defaults when `create`
/// is set, an outdated one is rewritten (the old one kept as
/// [`backup_path`]). `Err` for a file that can't be read, is malformed or
/// read-only, or can't be written; the file is then left as it is.
pub fn update_file(path: &Path, shipped: &str, create: bool) -> Result<FileUpdate, String> {
    let cannot_write = |e: std::io::Error| format!("unable to write {} ({e})", path.display());
    let Some(user) = read_text(path)? else {
        if !create || shipped.trim().is_empty() {
            return Ok(FileUpdate::Missing);
        }
        write_atomic(path, shipped).map_err(cannot_write)?;
        return Ok(FileUpdate::Created);
    };
    if shipped.trim().is_empty() {
        return Ok(FileUpdate::Unchanged);
    }
    let r = refresh(&user, shipped).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(notice) = r.notice() else {
        return Ok(FileUpdate::Unchanged);
    };
    refuse_read_only(path)?;
    write_backup(path, &user)?;
    write_atomic(path, &r.text).map_err(cannot_write)?;
    Ok(FileUpdate::Refreshed {
        notice,
        added: r.added.len(),
        dropped: r.dropped.len() + r.kept.len(),
    })
}

/// Load `path` over `defaults` for a mod: bring the file up to date
/// ([`update_file`], creating it if missing), then read it. A malformed file
/// is reported and the defaults are used; an unknown setting is skipped and
/// a value of the wrong type falls back to its default, each with one
/// warning, and every other value is used. Only invalid *built-in* defaults
/// are an error.
pub fn load<T: DeserializeOwned>(path: &Path, defaults: &str) -> Result<T, String> {
    let base: toml::Table = defaults
        .parse()
        .map_err(|e| format!("built-in defaults are invalid: {e}"))?;
    match update_file(path, defaults, true) {
        Ok(FileUpdate::Created) => log::info!("config created"),
        Ok(FileUpdate::Refreshed { notice, .. }) => log::info!("{notice}"),
        Ok(FileUpdate::Missing | FileUpdate::Unchanged) => {}
        Err(e) => log::warn!("unable to update config ({e})"),
    }
    let user = match read_text(path) {
        Ok(Some(text)) => match text.parse::<toml::Table>() {
            Ok(user) => user,
            Err(e) => {
                log::error!(
                    "unable to read config ({}); using defaults",
                    de_error(&text, &e)
                );
                toml::Table::new()
            }
        },
        Ok(None) => toml::Table::new(),
        Err(e) => {
            log::error!("unable to read config ({e}); using defaults");
            toml::Table::new()
        }
    };
    overlay(base, user).ok_or_else(|| "built-in defaults are invalid".to_owned())
}

/// Why a value of the player's was refused, judged against the type of the
/// setting's default: `not a whole number`, `not a number`, `not true or
/// false`, `not text`, `not a list`; `not valid` for a value of the right
/// type the mod still refused (out of its range, not one of its words).
fn problem(value: &toml::Value, default: Option<&toml::Value>) -> &'static str {
    use toml::Value as V;
    match (value, default) {
        (V::Integer(_), Some(V::Integer(_)))
        | (V::Float(_) | V::Integer(_), Some(V::Float(_)))
        | (V::Boolean(_), Some(V::Boolean(_)))
        | (V::String(_), Some(V::String(_)))
        | (V::Array(_), Some(V::Array(_)))
        | (_, None) => "is not valid",
        (_, Some(V::Integer(_))) => "is not a whole number",
        (_, Some(V::Float(_))) => "is not a number",
        (_, Some(V::Boolean(_))) => "is not true or false",
        (_, Some(V::String(_))) => "is not text",
        (_, Some(V::Array(_))) => "is not a list",
        _ => "is not valid",
    }
}

/// A value as written, on one line (a multi-line list or a `[table]`
/// collapsed), for a notice.
fn one_line_value(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `user` over `base`, key by key: unknown keys and values `T` rejects are
/// left out with a warning each. `None` only if `base` itself is rejected.
fn overlay<T: DeserializeOwned>(base: toml::Table, user: toml::Table) -> Option<T> {
    let mut known = toml::Table::new();
    for (k, v) in user {
        if base.contains_key(&k) {
            known.insert(k, v);
        } else {
            log::warn!("config: {k} unknown; skipped");
        }
    }
    let mut merged = base.clone();
    merged.extend(known.clone());
    if let Ok(v) = toml::Value::Table(merged).try_into() {
        return Some(v);
    }
    // Some value is rejected: take the player's values one at a time.
    let mut good = base.clone();
    for (k, v) in known {
        let mut trial = good.clone();
        trial.insert(k.clone(), v.clone());
        if toml::Value::Table(trial.clone()).try_into::<T>().is_ok() {
            good = trial;
        } else {
            let why = problem(&v, base.get(&k));
            let default = base.get(&k).map_or_default(ToString::to_string);
            log::warn!("config: {k} = {v} {why}; using {default}");
        }
    }
    toml::Value::Table(good)
        .try_into()
        .ok()
        .or_else(|| toml::Value::Table(base).try_into().ok())
}

/// The keys a settings type reads: the field names serde asks for when it
/// deserializes `T` (a struct, `#[serde(rename)]`s applied). `None` for a
/// type that is not a plain struct (a map, or a struct with a flattened
/// part), whose keys can't be listed.
#[must_use]
pub fn read_keys<T: DeserializeOwned>() -> Option<&'static [&'static str]> {
    use serde::de::{self, Visitor};
    use std::cell::Cell;

    /// Records the field list serde passes to `deserialize_struct`, then
    /// stops the deserialization with an error.
    struct Fields<'a>(&'a Cell<Option<&'static [&'static str]>>);

    #[derive(Debug)]
    struct Stop;
    impl std::fmt::Display for Stop {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("field list taken")
        }
    }
    impl std::error::Error for Stop {}
    impl de::Error for Stop {
        fn custom<M: std::fmt::Display>(_: M) -> Self {
            Stop
        }
    }

    impl<'de> de::Deserializer<'de> for Fields<'_> {
        type Error = Stop;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Stop> {
            Err(Stop)
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Stop> {
            self.0.set(Some(fields));
            Err(Stop)
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }

    let seen = Cell::new(None);
    let _ = T::deserialize(Fields(&seen));
    seen.get()
}

/// The check every mod's test runs: the shipped defaults list exactly the
/// keys `T` reads, so a refresh (which drops what the shipped file lacks)
/// never drops a key the mod uses, and no shipped key is dead. `retired`:
/// keys `T` still accepts that are deliberately not shipped (an old file's,
/// read by nothing). `Err` names every key on either side.
pub fn check_shipped_keys<T: DeserializeOwned>(
    shipped: &str,
    retired: &[&str],
) -> Result<(), String> {
    let read = read_keys::<T>().ok_or("the settings type is not a plain struct")?;
    let table: toml::Table = shipped
        .parse()
        .map_err(|e| format!("built-in defaults are invalid: {e}"))?;
    let missing: Vec<&str> = read
        .iter()
        .copied()
        .filter(|k| !table.contains_key(*k) && !retired.contains(k))
        .collect();
    let dead: Vec<&str> = table
        .keys()
        .map(String::as_str)
        .filter(|k| !read.contains(k))
        .collect();
    let stale: Vec<&str> = retired
        .iter()
        .copied()
        .filter(|k| !read.contains(k) || table.contains_key(*k))
        .collect();
    if missing.is_empty() && dead.is_empty() && stale.is_empty() {
        return Ok(());
    }
    Err(format!(
        "read but not shipped: {missing:?}; shipped but not read: {dead:?}; \
         listed as retired but read-and-shipped or not read: {stale:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)] // fields only listed, never used
    struct Keys {
        a: f64,
        #[serde(default)]
        b: bool,
        #[serde(rename = "c_renamed", default)]
        c: String,
    }

    #[test]
    fn read_keys_lists_struct_fields() {
        assert_eq!(read_keys::<Keys>(), Some(&["a", "b", "c_renamed"][..]));
        assert_eq!(read_keys::<toml::Table>(), None);
        assert!(check_shipped_keys::<Keys>("a = 1.0\nb = true\nc_renamed = \"\"\n", &[]).is_ok());
        let e = check_shipped_keys::<Keys>("a = 1.0\nb = true\nold = 1\n", &[]).unwrap_err();
        assert!(
            e.contains("[\"c_renamed\"]") && e.contains("[\"old\"]"),
            "{e}"
        );
        assert!(check_shipped_keys::<Keys>("a = 1.0\nb = true\n", &["c_renamed"]).is_ok());
    }

    #[derive(Deserialize, PartialEq, Debug)]
    #[serde(deny_unknown_fields)]
    struct Cfg {
        a: f64,
        b: bool,
    }

    const DEFAULTS: &str = "# comment kept\na = 2.0\nb = true\n";

    fn scratch(name: &str) -> PathBuf {
        // Scratch space under `target/test-tmp`.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ndml-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_overlays_and_recovers() {
        let dir = scratch("load");
        let path = dir.join("m.toml");

        let first: Cfg = load(&path, DEFAULTS).unwrap();
        assert_eq!(first, Cfg { a: 2.0, b: true });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULTS);

        // A partial file: b added at its default, a kept.
        std::fs::write(&path, "a = 5.0\n").unwrap();
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 5.0, b: true }
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# comment kept\na = 5.0\nb = true\n"
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "a = 5.0\n"
        );

        // A wrong type: only that value falls back; the others stay.
        std::fs::write(&path, "# comment kept\na = \"oops\"\nb = false\n").unwrap();
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 2.0, b: false }
        );
        // The file keeps the player's (wrong) value: nothing is lost.
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("a = \"oops\"")
        );

        // An unknown key: skipped, the rest used (and dropped from the file).
        std::fs::write(&path, "a = 7.0\nold = 1\nb = false\n").unwrap();
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 7.0, b: false }
        );
        assert!(!std::fs::read_to_string(&path).unwrap().contains("old"));

        // A syntax error: defaults, file left alone.
        std::fs::write(&path, "a = [nonsense").unwrap();
        assert_eq!(
            load::<Cfg>(&path, DEFAULTS).unwrap(),
            Cfg { a: 2.0, b: true }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = [nonsense");

        assert!(parse_defaults::<Cfg>("a = 1").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refresh_keeps_values_and_formatting() {
        let shipped = "# About.\n\n# How far.\nrange = 3.0\n# Mode.\n# options: a, b\nmode = \"a\"\nlist = [\"x\", \"y\"]\nnew_one = 20\n";
        let user =
            "# old about\nrange = 1.50 # mine\nmode = 'b'\nlist = [ \"z\" ]\nold_radius = 20\n";
        let r = refresh(user, shipped).unwrap();
        assert_eq!(
            r.text,
            "# About.\n\n# How far.\nrange = 1.50\n# Mode.\n# options: a, b\nmode = 'b'\nlist = [ \"z\" ]\nnew_one = 20\n"
        );
        assert!(r.changed);
        assert_eq!(
            r.notice().unwrap(),
            "config refreshed: new_one added (20), old_radius dropped (was 20)"
        );
        // Twice: nothing more to do.
        let again = refresh(&r.text, shipped).unwrap();
        assert!(!again.changed && again.notice().is_none());
        // Values alone differ: nothing to do (CRLF files too).
        let only_values = shipped.replace("3.0", "9.5").replace('\n', "\r\n");
        assert!(refresh(&only_values, shipped).unwrap().notice().is_none());
        // Comments alone differ.
        let old_text = shipped.replace("# How far.", "# How far, in px.");
        assert_eq!(
            refresh(&old_text, shipped).unwrap().notice().unwrap(),
            "config refreshed: comments updated"
        );
        // Malformed player file: an error, nothing produced.
        assert!(refresh("range = [", shipped).is_err());
        assert!(refresh(user, "[t]\na = 1\n").is_err());
    }

    /// The other policy for retired settings: kept at the end, marked, and
    /// nothing more to do the second time.
    #[test]
    fn retired_settings_can_be_kept_as_unused() {
        let shipped = "# A.\na = 1\n";
        let r = refresh_with("a = 5\nold = true\n", shipped, Retired::KeepUnused).unwrap();
        assert_eq!(
            r.text,
            "# A.\na = 5\n\n# No longer used by this mod.\nold = true\n"
        );
        assert_eq!(r.notice().unwrap(), "config refreshed: old unused");
        let again = refresh_with(&r.text, shipped, Retired::KeepUnused).unwrap();
        assert!(!again.changed);
        assert_eq!(RETIRED, Retired::Drop);
    }

    /// After several updates in a row the result only depends on the last
    /// shipped file.
    #[test]
    fn refresh_compares_with_the_current_shipped_file_only() {
        let v1 = "# v1\na = 1\nb = 2\n";
        let v2 = "# v2\na = 1\nc = 3\n";
        let v3 = "# v3\nc = 3\nd = 4\na = 1\n";
        let mine = "# v1\na = 10\nb = 2\n";
        let step = refresh(&refresh(mine, v2).unwrap().text, v3).unwrap().text;
        let direct = refresh(mine, v3).unwrap();
        assert_eq!(step, direct.text);
        assert_eq!(direct.text, "# v3\nc = 3\nd = 4\na = 10\n");
        assert_eq!(
            direct.notice().unwrap(),
            "config refreshed: c added (3), d added (4), b dropped (was 2)"
        );
        assert!(refresh(v1, v1).unwrap().notice().is_none());
    }

    #[test]
    fn update_file_backs_up_and_leaves_bad_files_alone() {
        let dir = scratch("update");
        let path = dir.join("m.toml");
        assert_eq!(
            update_file(&path, DEFAULTS, false).unwrap(),
            FileUpdate::Missing
        );
        assert!(!path.exists(), "viewing writes nothing");
        assert_eq!(
            update_file(&path, DEFAULTS, true).unwrap(),
            FileUpdate::Created
        );
        assert_eq!(
            update_file(&path, DEFAULTS, true).unwrap(),
            FileUpdate::Unchanged
        );
        std::fs::write(&path, "b = false\n").unwrap();
        assert_eq!(
            update_file(&path, DEFAULTS, true).unwrap(),
            FileUpdate::Refreshed {
                notice: "config refreshed: a added (2.0)".into(),
                added: 1,
                dropped: 0
            }
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "b = false\n"
        );
        assert!(!dir.join("m.toml.tmp").exists());
        // Malformed: reported, untouched.
        std::fs::write(&path, "b = \n").unwrap();
        assert!(update_file(&path, DEFAULTS, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b = \n");
        // Read-only: reported, untouched.
        std::fs::write(&path, "b = false\n").unwrap();
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&path, p.clone()).unwrap();
        assert!(update_file(&path, DEFAULTS, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b = false\n");
        #[allow(clippy::permissions_set_readonly_false)] // Windows: clears the attribute only
        p.set_readonly(false);
        std::fs::set_permissions(&path, p).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn set_read_only(path: &Path, on: bool) {
        let mut p = std::fs::metadata(path).unwrap().permissions();
        p.set_readonly(on);
        std::fs::set_permissions(path, p).unwrap();
    }

    /// What goes wrong is said in one line that names the file: a malformed
    /// file with its line, a read-only one with the fix, a file that is not
    /// UTF-8. The SDK's own read-only leftovers (`.tmp`, `.bak`) do not stop a
    /// write; the player's read-only file is never written.
    #[test]
    fn failures_name_the_file_in_one_line() {
        let dir = scratch("failures");
        let path = dir.join("m.toml");
        let one_line = |e: &str| assert!(!e.contains('\n'), "{e}");

        std::fs::write(&path, "a = 1.0\na = 2.0\n").unwrap();
        let e = update_file(&path, DEFAULTS, true).unwrap_err();
        one_line(&e);
        assert!(e.contains("m.toml: not valid TOML (line 2: "), "{e}");

        std::fs::write(&path, [b'a', b' ', b'=', b' ', 0xff, b'\n']).unwrap();
        let e = update_file(&path, DEFAULTS, true).unwrap_err();
        assert!(e.ends_with("m.toml is not UTF-8 text"), "{e}");

        std::fs::write(&path, "b = false\n").unwrap();
        set_read_only(&path, true);
        let e = update_file(&path, DEFAULTS, true).unwrap_err();
        one_line(&e);
        assert!(
            e.contains("m.toml is read-only; clear its read-only attribute to change it"),
            "{e}"
        );
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        set_read_only(&path, false);

        // The SDK's read-only leftovers: replaced.
        let (tmp, bak) = (dir.join("m.toml.tmp"), backup_path(&path));
        for f in [&tmp, &bak] {
            std::fs::write(f, "old").unwrap();
            set_read_only(f, true);
        }
        assert!(matches!(
            update_file(&path, DEFAULTS, true).unwrap(),
            FileUpdate::Refreshed { .. }
        ));
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "b = false\n");
        assert!(!tmp.exists());
        set_read_only(&bak, false);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A player's `[table]` where the mod ships a plain setting can't be
    /// kept: the notice names it as dropped with what it held.
    #[test]
    fn a_table_in_place_of_a_setting_is_named_as_dropped() {
        let r = refresh("a = 5.0\n\n[b]\nc = 1\nd = 2\n", DEFAULTS).unwrap();
        assert_eq!(r.text, "# comment kept\na = 5.0\nb = true\n");
        let notice = r.notice().unwrap();
        assert!(notice.contains("b added (true)"), "{notice}");
        assert!(notice.contains("b dropped (was "), "{notice}");
        assert!(notice.contains("c = 1"), "{notice}");
        assert!(!notice.contains('\n'), "{notice}");
    }

    /// The warning for a refused value names the problem by the setting's
    /// type, one wording per idea.
    #[test]
    fn a_refused_value_names_its_problem() {
        use toml::Value as V;
        let s = |t: &str| V::String(t.into());
        assert_eq!(
            problem(&s("x"), Some(&V::Integer(1))),
            "is not a whole number"
        );
        assert_eq!(problem(&s("x"), Some(&V::Float(1.0))), "is not a number");
        assert_eq!(
            problem(&V::Integer(1), Some(&V::Float(1.0))),
            "is not valid"
        );
        assert_eq!(
            problem(&s("yes"), Some(&V::Boolean(true))),
            "is not true or false"
        );
        assert_eq!(problem(&V::Integer(3), Some(&s("a"))), "is not text");
        assert_eq!(problem(&s("a"), Some(&V::Array(vec![]))), "is not a list");
        assert_eq!(problem(&s("loud"), Some(&s("quiet"))), "is not valid");
    }
}
