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

//! Finding, checking and launching the game.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ndml_common::GAME_EXE;

/// `CREATE_NO_WINDOW`: don't flash a console for helper processes.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const STEAM_ROOTS: [&str; 2] = [r"C:\Program Files (x86)\Steam", r"C:\Program Files\Steam"];
const GAME_FOLDER: &str = "Nova Drift";

/// Resolve the game directory: explicit path, then `NOVA_DRIFT_DIR`, then Steam libraries.
#[must_use]
pub fn locate(explicit: Option<&Path>) -> Option<PathBuf> {
    let candidates = explicit
        .map(Path::to_path_buf)
        .into_iter()
        .chain(std::env::var_os("NOVA_DRIFT_DIR").map(PathBuf::from))
        .chain(steam_game_dirs());
    candidates.into_iter().find(|d| d.join(GAME_EXE).is_file())
}

fn steam_game_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in STEAM_ROOTS {
        let root = Path::new(root);
        out.push(root.join("steamapps").join("common").join(GAME_FOLDER));
        if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf"))
        {
            out.extend(
                library_paths(&vdf)
                    .into_iter()
                    .map(|p| p.join("steamapps").join("common").join(GAME_FOLDER)),
            );
        }
    }
    out
}

/// Extract `"path"  "D:\\SteamLibrary"` entries from `libraryfolders.vdf`.
#[must_use]
pub fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut quoted = line.split('"').skip(1).step_by(2);
            (quoted.next()? == "path")
                .then(|| quoted.next().map(|p| PathBuf::from(p.replace(r"\\", r"\"))))?
        })
        .collect()
}

/// Whether a `NovaDrift.exe` process is running (asks `tasklist`; ~50 ms).
#[must_use]
pub fn is_running() -> bool {
    Command::new("tasklist")
        .args([
            "/FI",
            &format!("IMAGENAME eq {GAME_EXE}"),
            "/FO",
            "CSV",
            "/NH",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| tasklist_lists_game(&String::from_utf8_lossy(&o.stdout)))
}

fn tasklist_lists_game(csv: &str) -> bool {
    let needle = format!("\"{}\"", GAME_EXE.to_ascii_lowercase());
    csv.lines()
        .any(|l| l.to_ascii_lowercase().starts_with(&needle))
}

/// Polls [`is_running`] on a background thread so the UI never blocks on it.
#[derive(Debug, Clone)]
pub struct RunningWatcher(Arc<AtomicBool>);

impl RunningWatcher {
    #[must_use]
    pub fn spawn() -> Self {
        let flag = Arc::new(AtomicBool::new(is_running()));
        let writer = Arc::clone(&flag);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(1500));
                writer.store(is_running(), Ordering::Relaxed);
            }
        });
        Self(flag)
    }

    #[must_use]
    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Nova Drift's Steam app id (from `appmanifest_858210.acf`).
pub const STEAM_APP_ID: u32 = 858_210;

/// Launch through Steam (`steam://rungameid/…`), which Steamworks requires;
/// started directly the game asks Steam to relaunch it. Falls back to running
/// the exe if the URL can't be opened.
pub fn launch(game_dir: &Path) -> std::io::Result<()> {
    let via_steam = Command::new("cmd")
        .args([
            "/C",
            "start",
            "",
            &format!("steam://rungameid/{STEAM_APP_ID}"),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    match via_steam {
        Ok(s) if s.success() => Ok(()),
        _ => Command::new(game_dir.join(GAME_EXE))
            .current_dir(game_dir)
            .spawn()
            .map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tasklist_output() {
        assert!(tasklist_lists_game(
            "\"NovaDrift.exe\",\"52320\",\"Console\",\"1\",\"1,190,044 K\"
"
        ));
        assert!(!tasklist_lists_game(
            "INFO: No tasks are running which match the specified criteria.
"
        ));
    }

    #[test]
    fn parses_library_folders() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            library_paths(vdf),
            vec![
                PathBuf::from(r"C:\Program Files (x86)\Steam"),
                PathBuf::from(r"D:\SteamLibrary")
            ]
        );
    }
}
