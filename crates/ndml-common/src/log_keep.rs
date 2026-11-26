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

//! The last launches' logs. The running game writes `logs/ndml.log`; before
//! it opens that file, the loader renames the one left by the launch before
//! to that session's start in the PC's local time,
//! `ndml-YYYY-MM-DD_HH-MM-SS.log`, and deletes the oldest of those so
//! [`KEEP`] files remain in all: the running one and nine.
//!
//! The local time comes from the loader ([`Clock`]; this crate makes no
//! system call for it). When it could not be read, names are in UTC with a
//! `Z`, `ndml-YYYY-MM-DD_HH-MM-SSZ.log`; pruning counts both forms (their
//! order may be off by the UTC offset).
//!
//! Only plain files in the logs folder itself whose names have exactly one
//! of those forms are ever renamed or deleted: no recursion, no links
//! followed, any other file left alone. Nothing here may stop the game: a
//! rename or delete that fails is skipped (the old log is then overwritten)
//! and reported back for one line in the new log.

#![forbid(unsafe_code)]

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The running session's log in the logs folder.
pub const LOG_NAME: &str = "ndml.log";
/// Log files kept in all: the running one and the last nine launches'.
pub const KEEP: usize = 10;
/// What the first line of a log says before the session's start time
/// ([`started_text`]), so the next launch can name the file by it.
const STARTED: &str = "session started ";
/// The largest UTC offset taken as real (UTC+14 is the largest in use).
const MAX_OFFSET: i64 = 15 * 3600;

/// What came of keeping the previous log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Kept {
    /// The name the previous `ndml.log` was given.
    pub renamed_to: Option<String>,
    /// Old logs deleted.
    pub deleted: Vec<String>,
    /// What could not be done, for one line in the new log.
    pub problems: Vec<String>,
}

/// This launch's clock: seconds since the Unix epoch, and the PC's offset
/// from UTC in seconds when its local time could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    pub utc: u64,
    pub offset: Option<i64>,
}

impl Clock {
    /// No local time: names in UTC with a `Z`.
    #[must_use]
    pub fn utc_only(utc: u64) -> Self {
        Self { utc, offset: None }
    }

    /// `local` (year, month, day, hour, minute, second) read at about the
    /// same moment as `utc`; the offset is rounded to the minute. A local
    /// time that is no real date, or more than 15 h away from UTC, is taken
    /// as unread.
    #[must_use]
    pub fn with_local(utc: u64, local: (u16, u16, u16, u16, u16, u16)) -> Self {
        let (y, mo, d, h, mi, s) = local;
        let real = y >= 1970
            && (1..=12).contains(&mo)
            && (1..=31).contains(&d)
            && h < 24
            && mi < 60
            && s < 60;
        let offset = real
            .then(|| {
                let secs = days_from_civil(u64::from(y), u64::from(mo), u64::from(d)) * 86_400
                    + u64::from(h) * 3600
                    + u64::from(mi) * 60
                    + u64::from(s);
                let off = i64::try_from(secs).ok()? - i64::try_from(utc).ok()?;
                Some((off + 30).div_euclid(60) * 60)
            })
            .flatten()
            .filter(|o| o.abs() <= MAX_OFFSET);
        Self { utc, offset }
    }

    /// The start of a session at `utc` as this clock names it: local when
    /// the offset is known (the offset now, for a session that may have
    /// started on the other side of a daylight-saving change), else UTC.
    fn start_of(&self, utc: u64) -> Start {
        self.offset
            .and_then(|o| u64::try_from(i64::try_from(utc).ok()? + o).ok())
            .map_or(Start::Utc(utc), Start::Local)
    }
}

/// A session's start as a kept log's name gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The PC's wall clock, counted in seconds as if it were UTC.
    Local(u64),
    /// UTC seconds since the Unix epoch.
    Utc(u64),
}

/// Seconds since the Unix epoch of `t` (0 before it).
#[must_use]
pub fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `2025-06-02 03:15:30` for `secs`.
fn wall(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// The new log's first line:
/// `session started 2025-06-02 03:15:30 local (2025-06-01 18:15:30 UTC)`,
/// or `session started 2025-06-01 18:15:30 UTC` without the local time.
#[must_use]
pub fn started_text(c: &Clock) -> String {
    match c.start_of(c.utc) {
        Start::Local(l) => format!("{STARTED}{} local ({} UTC)", wall(l), wall(c.utc)),
        Start::Utc(u) => format!("{STARTED}{} UTC", wall(u)),
    }
}

/// The kept name for a session that started at `start`:
/// `ndml-2025-06-02_03-15-30.log` (local) or `ndml-2025-06-01_18-15-30Z.log`
/// (UTC); `-2`, `-3`, ... before `.log` for another session in the same
/// second.
#[must_use]
#[allow(clippy::many_single_char_names)] // the date's and time's fields
pub fn kept_name(start: Start, n: u32) -> String {
    let (secs, z) = match start {
        Start::Local(s) => (s, ""),
        Start::Utc(s) => (s, "Z"),
    };
    let (y, mo, d, h, mi, s) = civil(secs);
    let more = if n > 1 {
        format!("-{n}")
    } else {
        String::new()
    };
    format!("ndml-{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}{z}{more}.log")
}

/// For a kept log's name (either form), its order (the date and time, then
/// the same second's number); `None` for any other name.
#[must_use]
pub fn kept_order(name: &str) -> Option<(&str, u32)> {
    let rest = name.strip_prefix("ndml-")?.strip_suffix(".log")?;
    let stamp = rest.get(..19)?;
    let shape = stamp.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 | 13 | 16 => b == b'-',
        10 => b == b'_',
        _ => b.is_ascii_digit(),
    });
    if !shape {
        return None;
    }
    let more = rest.get(19..)?;
    let more = more.strip_prefix('Z').unwrap_or(more);
    let n = match more {
        "" => 1,
        more => {
            let digits = more.strip_prefix('-')?;
            if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            digits.parse().ok().filter(|&n| n > 1)?
        }
    };
    Some((stamp, n))
}

/// `YYYY-MM-DD HH:MM:SS` at the start of `s`, in seconds.
#[allow(clippy::many_single_char_names)] // the date's and time's fields
fn parse_wall(s: &str) -> Option<u64> {
    let s = s.get(..19)?;
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let b = s.as_bytes();
    let ok = b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && y >= 1970
        && (1..=12).contains(&mo)
        && (1..=31).contains(&d)
        && h < 24
        && mi < 60
        && sec < 60;
    ok.then(|| days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// The start a log's first line gives ([`started_text`]): its local part,
/// or its UTC time on a line without one.
fn first_line_start(path: &Path) -> Option<Start> {
    let mut head = [0u8; 512];
    let n = fs::File::open(path).ok()?.read(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);
    let line = text.lines().next()?;
    let at = line.find(STARTED)? + STARTED.len();
    let rest = line.get(at..)?;
    let secs = parse_wall(rest)?;
    let after = rest.get(19..)?;
    if after.starts_with(" local") {
        Some(Start::Local(secs))
    } else if after.starts_with(" UTC") {
        Some(Start::Utc(secs))
    } else {
        None
    }
}

/// Keep the previous launch's `ndml.log` in `logs` and prune the kept ones
/// (see the module docs). `now` is this launch's clock: it names a previous
/// log in local time (a UTC start line or a last write converted with
/// today's offset), and gives the start of one that cannot be told any other
/// way.
#[must_use]
pub fn keep_previous(logs: &Path, now: &Clock) -> Kept {
    let mut out = Kept::default();
    let current = logs.join(LOG_NAME);
    match fs::symlink_metadata(&current) {
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => out
            .problems
            .push(format!("could not look at the previous {LOG_NAME} ({e})")),
        Ok(m) if !m.file_type().is_file() => out.problems.push(format!(
            "the previous {LOG_NAME} is not a plain file; not renamed"
        )),
        Ok(m) => {
            // The start its first line gives. A log without a start line is
            // named by its last write: its creation time can be older than
            // its session (a file truncated by each launch keeps the first).
            let start = match first_line_start(&current) {
                Some(Start::Utc(u)) => now.start_of(u),
                Some(local) => local,
                None => now.start_of(m.modified().ok().map_or(now.utc, unix_secs)),
            };
            let free = (1..=99)
                .map(|n| kept_name(start, n))
                .find(|name| fs::symlink_metadata(logs.join(name)).is_err());
            match free {
                None => out.problems.push(format!(
                    "no free name to keep the previous {LOG_NAME}; it is overwritten"
                )),
                Some(name) => match fs::rename(&current, logs.join(&name)) {
                    Ok(()) => out.renamed_to = Some(name),
                    Err(e) => out.problems.push(format!(
                        "could not rename the previous {LOG_NAME} to {name} ({e}); it is overwritten"
                    )),
                },
            }
        }
    }
    prune(logs, &mut out);
    out
}

/// Delete the oldest kept logs so `KEEP - 1` remain beside the running one.
fn prune(logs: &Path, out: &mut Kept) {
    let entries = match fs::read_dir(logs) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) => {
            out.problems
                .push(format!("could not list the logs folder ({e})"));
            return;
        }
    };
    let mut kept: Vec<String> = entries
        .filter_map(Result::ok)
        // `file_type` does not follow links: a link or folder is left alone.
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| kept_order(name).is_some())
        .collect();
    kept.sort_by(|a, b| kept_order(a).cmp(&kept_order(b)));
    let excess = kept.len().saturating_sub(KEEP - 1);
    for name in kept.into_iter().take(excess) {
        match fs::remove_file(logs.join(&name)) {
            Ok(()) => out.deleted.push(name),
            Err(e) => out.problems.push(format!("could not delete {name} ({e})")),
        }
    }
}

/// Calendar date and time of `secs` since 1970 (Howard Hinnant's
/// days-to-civil algorithm).
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // in range by construction
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    #[allow(clippy::cast_sign_loss)] // 1..=31 and 1..=12
    let (d, m) = (
        (doy - (153 * mp + 2) / 5 + 1) as u32,
        if mp < 10 { mp + 3 } else { mp - 9 } as u32,
    );
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Days since 1970 of a date from 1970 on (the inverse of [`civil`]).
fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y.saturating_sub(1) } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).saturating_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh scratch folder under `target/test-tmp/`.
    fn scratch(name: &str) -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/log-keep")
            .join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn names(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    /// 2025-06-01 18:15:30 UTC: 2025-06-02 03:15:30 in a zone at UTC+9.
    const T: u64 = 1_748_801_730;
    const OFFSET: i64 = 9 * 3600;

    fn zoned(utc: u64) -> Clock {
        Clock {
            utc,
            offset: Some(OFFSET),
        }
    }

    fn local(utc: u64) -> String {
        kept_name(zoned(utc).start_of(utc), 1)
    }

    fn z(utc: u64) -> String {
        kept_name(Start::Utc(utc), 1)
    }

    #[test]
    fn names_and_the_first_line_in_both_forms() {
        assert_eq!(local(T), "ndml-2025-06-02_03-15-30.log");
        assert_eq!(z(T), "ndml-2025-06-01_18-15-30Z.log");
        assert_eq!(
            kept_name(Start::Local(T), 2),
            "ndml-2025-06-01_18-15-30-2.log"
        );
        assert_eq!(
            kept_name(Start::Utc(T), 2),
            "ndml-2025-06-01_18-15-30Z-2.log"
        );
        assert_eq!(z(0), "ndml-1970-01-01_00-00-00Z.log");
        assert_eq!(z(951_782_400), "ndml-2000-02-29_00-00-00Z.log");
        assert_eq!(
            started_text(&zoned(T)),
            "session started 2025-06-02 03:15:30 local (2025-06-01 18:15:30 UTC)"
        );
        assert_eq!(
            started_text(&Clock::utc_only(T)),
            "session started 2025-06-01 18:15:30 UTC"
        );
        for secs in [0, 951_782_400, T, 4_102_444_799] {
            assert_eq!(days_from_civil_of(secs), secs / 86_400 * 86_400);
        }
        for (name, stamp, n) in [
            ("ndml-2025-06-02_03-15-30.log", "2025-06-02_03-15-30", 1),
            ("ndml-2025-06-02_03-15-30-3.log", "2025-06-02_03-15-30", 3),
            ("ndml-2025-06-01_18-15-30Z.log", "2025-06-01_18-15-30", 1),
            (
                "ndml-2025-06-01_18-15-30Z-12.log",
                "2025-06-01_18-15-30",
                12,
            ),
        ] {
            assert_eq!(kept_order(name), Some((stamp, n)), "{name}");
        }
        for foreign in [
            "ndml.log",
            "ndml-2025-06-02_04-10.log",
            "ndml-2025-06-02_04-10-45Z-1.log",
            "ndml-2025-06-02_04-10-45-1.log",
            "ndml-2025-06-02_04-10-45Z-.log",
            "ndml-2025-06-02_04-10-45ZZ.log",
            "ndml-2025-06-02_04-10-45x.log",
            "ndml-2025-06-02_04-10-45Z.log.txt",
            "ndml-2025-06-02 04-10-45.log",
            "ndml-2025-06-02_04-10-4x.log",
            "xndml-2025-06-02_04-10-45.log",
            "notes.txt",
        ] {
            assert_eq!(kept_order(foreign), None, "{foreign}");
        }
    }

    fn days_from_civil_of(secs: u64) -> u64 {
        let (y, m, d, ..) = civil(secs);
        days_from_civil(y as u64, u64::from(m), u64::from(d)) * 86_400
    }

    /// The loader's local time read: the offset to the minute; no real
    /// date, or a wild offset, is no local time.
    #[test]
    fn the_offset_comes_from_the_local_time() {
        assert_eq!(
            Clock::with_local(T, (2025, 6, 2, 3, 15, 30)).offset,
            Some(OFFSET)
        );
        // Read a second apart: still the whole offset.
        assert_eq!(
            Clock::with_local(T + 1, (2025, 6, 2, 3, 15, 30)).offset,
            Some(OFFSET)
        );
        assert_eq!(
            Clock::with_local(T, (2025, 6, 1, 23, 45, 30)).offset,
            Some(5 * 3600 + 1800)
        );
        for bad in [
            (0, 0, 0, 0, 0, 0),
            (2025, 13, 1, 0, 0, 0),
            (2025, 6, 2, 24, 0, 0),
            (2025, 6, 3, 18, 15, 30),
        ] {
            assert_eq!(Clock::with_local(T, bad).offset, None, "{bad:?}");
        }
    }

    #[test]
    fn the_previous_log_is_named_by_the_local_part_of_its_first_line() {
        let d = scratch("first-line");
        fs::write(
            d.join(LOG_NAME),
            format!(
                "[    0.000] INFO  ndml: {}\nmore\n",
                started_text(&zoned(T))
            ),
        )
        .unwrap();
        // Read on a PC whose offset is now another: the line's local part.
        let k = keep_previous(&d, &Clock::utc_only(T + 600));
        assert_eq!(
            k.renamed_to.as_deref(),
            Some("ndml-2025-06-02_03-15-30.log")
        );
        assert!(k.problems.is_empty() && k.deleted.is_empty(), "{k:?}");
        // A second session in the same second does not overwrite it.
        fs::write(d.join(LOG_NAME), format!("{}\n", started_text(&zoned(T)))).unwrap();
        let k = keep_previous(&d, &zoned(T + 600));
        assert_eq!(
            k.renamed_to.as_deref(),
            Some("ndml-2025-06-02_03-15-30-2.log")
        );
        assert_eq!(
            names(&d),
            [
                "ndml-2025-06-02_03-15-30-2.log",
                "ndml-2025-06-02_03-15-30.log"
            ]
        );
    }

    /// A UTC-only start line: local with today's offset; with no local
    /// time, UTC with a `Z`.
    #[test]
    fn a_utc_start_line_is_named_in_local_time_or_with_a_z() {
        let d = scratch("utc-line");
        let utc_line = format!("{}\n", started_text(&Clock::utc_only(T)));
        fs::write(d.join(LOG_NAME), &utc_line).unwrap();
        let k = keep_previous(&d, &zoned(T + 600));
        assert_eq!(
            k.renamed_to.as_deref(),
            Some("ndml-2025-06-02_03-15-30.log")
        );
        fs::write(d.join(LOG_NAME), &utc_line).unwrap();
        let k = keep_previous(&d, &Clock::utc_only(T + 600));
        assert_eq!(
            k.renamed_to.as_deref(),
            Some("ndml-2025-06-01_18-15-30Z.log")
        );
    }

    #[test]
    fn a_log_without_a_start_line_is_named_by_its_last_write() {
        let d = scratch("no-start");
        for clock in [zoned(0), Clock::utc_only(0)] {
            fs::write(d.join(LOG_NAME), "[    0.012] INFO  ndml: old format\n").unwrap();
            let before = unix_secs(SystemTime::now());
            let k = keep_previous(&d, &clock);
            let name = k.renamed_to.unwrap();
            let after = unix_secs(SystemTime::now());
            assert!(
                (before.saturating_sub(2)..=after).any(|s| kept_name(clock.start_of(s), 1) == name),
                "{name}"
            );
            assert_eq!(name.contains('Z'), clock.offset.is_none(), "{name}");
        }
    }

    #[test]
    fn eleven_kept_logs_of_both_forms_are_pruned_to_nine_and_foreign_files_stay() {
        let d = scratch("prune");
        // Hours 0..=5 named in UTC, 6..=10 in local time.
        for i in 0..11 {
            let t = T + i * 3600;
            let name = if i < 6 { z(t) } else { local(t + 24 * 3600) };
            fs::write(d.join(name), "x").unwrap();
        }
        // Not kept logs: never deleted, never counted.
        for foreign in ["notes.txt", "ndml-2020-01-01_00-00.log", "ndml.log.bak"] {
            fs::write(d.join(foreign), "x").unwrap();
        }
        fs::create_dir(d.join("ndml-2020-01-01_00-00-00.log")).unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub").join(z(0)), "x").unwrap();
        fs::write(
            d.join(LOG_NAME),
            format!("{}\n", started_text(&zoned(T + 40 * 3600))),
        )
        .unwrap();
        let k = keep_previous(&d, &zoned(T + 41 * 3600));
        assert!(k.problems.is_empty(), "{k:?}");
        // Twelve kept now: the three oldest go.
        assert_eq!(k.deleted, [z(T), z(T + 3600), z(T + 7200)]);
        let left = names(&d);
        let ours: Vec<&String> = left
            .iter()
            .filter(|n| kept_order(n).is_some() && d.join(n).is_file())
            .collect();
        assert_eq!(ours.len(), KEEP - 1, "{left:?}");
        for foreign in [
            "notes.txt",
            "ndml-2020-01-01_00-00.log",
            "ndml.log.bak",
            "ndml-2020-01-01_00-00-00.log",
            "sub",
        ] {
            assert!(
                left.iter().any(|n| n == foreign),
                "{foreign} kept: {left:?}"
            );
        }
        assert!(d.join("sub").join(z(0)).exists(), "no recursion");
    }

    #[test]
    fn fewer_than_ten_are_all_kept_and_a_missing_or_empty_folder_is_fine() {
        let d = scratch("few");
        for i in 0..5 {
            fs::write(d.join(local(T + i)), "x").unwrap();
        }
        let k = keep_previous(&d, &zoned(T));
        assert_eq!(k, Kept::default());
        assert_eq!(names(&d).len(), 5);
        let e = scratch("empty");
        assert_eq!(keep_previous(&e, &zoned(T)), Kept::default());
        let gone = e.join("missing");
        assert_eq!(keep_previous(&gone, &zoned(T)), Kept::default());
    }

    /// The same second's numbers sort after it, so pruning takes the right
    /// one first.
    #[test]
    fn a_same_second_collision_prunes_in_order() {
        let d = scratch("same-second");
        fs::write(d.join(local(T)), "x").unwrap();
        fs::write(d.join(kept_name(zoned(T).start_of(T), 2)), "x").unwrap();
        for i in 1..9 {
            fs::write(d.join(local(T + i)), "x").unwrap();
        }
        let k = keep_previous(&d, &zoned(T));
        assert_eq!(k.deleted, [local(T)]);
        assert!(d.join(kept_name(zoned(T).start_of(T), 2)).exists());
    }

    /// A previous log another process holds open without delete sharing
    /// (Windows) cannot be renamed: skipped, reported; pruning still runs.
    /// A kept log held so cannot be deleted: skipped, reported.
    #[cfg(windows)]
    #[test]
    fn a_locked_log_is_skipped_and_reported() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = scratch("locked");
        fs::write(
            d.join(LOG_NAME),
            format!("{}\n", started_text(&zoned(T + 99))),
        )
        .unwrap();
        for i in 0..10 {
            fs::write(d.join(local(T + i)), "x").unwrap();
        }
        let open_locked = |p: PathBuf| {
            fs::OpenOptions::new()
                .read(true)
                .share_mode(1) // FILE_SHARE_READ only: no rename, no delete
                .open(p)
                .unwrap()
        };
        let _held = open_locked(d.join(LOG_NAME));
        let _held_old = open_locked(d.join(local(T)));
        let k = keep_previous(&d, &zoned(T + 100));
        assert_eq!(k.renamed_to, None);
        assert_eq!(k.problems.len(), 2, "{k:?}");
        assert!(
            k.problems[0].starts_with("could not rename the previous ndml.log to ")
                && k.problems[0].ends_with("; it is overwritten"),
            "{k:?}"
        );
        assert!(k.problems[1].starts_with(&format!("could not delete {}", local(T))));
        // Skipped, not made up for by deleting another.
        assert!(k.deleted.is_empty());
        assert!(d.join(LOG_NAME).exists() && d.join(local(T)).exists());
        assert_eq!(names(&d).len(), 11);
    }
}
