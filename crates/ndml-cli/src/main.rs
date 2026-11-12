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

//! `ndml` — the NDML manager.
//!
//! With no command it opens the retro terminal UI. Commands exist for scripts:
//!
//! ```text
//! ndml [--game-dir <path>] [tui|install|uninstall|status|launch|snapshot N [MOD]]
//! ```
//!
//! Artifacts are deployed from the directory containing `ndml.exe`
//! (`target/release` after `cargo build --release`).

#![forbid(unsafe_code)]
// UI layout converts between cell counts and floats, and each screen has one
// long draw function.
#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

mod config_edit;
mod deploy;
mod game;
mod modinfo;
mod tui;

use std::path::PathBuf;
use std::process::ExitCode;

use ndml_common::Paths;

const USAGE: &str = "\
ndml — Nova Drift Mod Loader manager

USAGE: ndml [--game-dir <path>] [COMMAND]

COMMANDS:
  tui        retro terminal UI (default)
  install    deploy loader (version.dll) and all built mods
  uninstall  remove the loader; mods/configs/logs under ndml/ are kept
  status     print deployment state
  launch     start Nova Drift
  snapshot N [MOD]  print tab N (1-4) of the UI as plain text, optionally with MOD's config open

The game folder is found via --game-dir, NOVA_DRIFT_DIR, or Steam libraries.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut game_dir: Option<PathBuf> = None;
    let mut command = String::from("tui");
    let mut snapshot_tab: Option<usize> = None;
    let mut snapshot_mod: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--game-dir" => game_dir = args.next().map(PathBuf::from),
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            n if command == "snapshot" && n.parse::<usize>().is_ok() => {
                snapshot_tab = n.parse().ok();
            }
            m if command == "snapshot" => snapshot_mod = Some(m.to_owned()),
            other => other.clone_into(&mut command),
        }
    }
    let game = game::locate(game_dir.as_deref());

    let result = match command.as_str() {
        "tui" => tui::run(game.as_deref()).map_err(|e| e.to_string()),
        "snapshot" => {
            let tab = snapshot_tab.unwrap_or(1);
            print!(
                "{}",
                tui::snapshot(game.as_deref(), tab, snapshot_mod.as_deref())
            );
            Ok(())
        }
        cmd => with_game(game, |paths| run_command(cmd, paths)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn with_game(
    game: Option<PathBuf>,
    f: impl FnOnce(&Paths) -> Result<(), String>,
) -> Result<(), String> {
    let game = game.ok_or("Nova Drift not found; pass --game-dir or set NOVA_DRIFT_DIR")?;
    f(&Paths::new(&game))
}

fn run_command(cmd: &str, paths: &Paths) -> Result<(), String> {
    let build = deploy::build_dir();
    match cmd {
        "install" => {
            for line in deploy::install(paths, &build)? {
                println!("deployed {line}");
            }
            Ok(())
        }
        "uninstall" => {
            deploy::uninstall(paths)?;
            println!("loader removed (kept {})", paths.root.display());
            Ok(())
        }
        "status" => {
            let settings = ndml_common::read(&paths.settings_file());
            println!("game:    {}", paths.game.display());
            println!("build:   {}", build.display());
            println!("loader:  {:?}", deploy::loader_state(paths, &build));
            println!("running: {}", game::is_running());
            for m in deploy::scan_mods(paths, &build, &settings) {
                let ver = m.meta.as_ref().map_or("?", |i| i.version.as_str());
                println!(
                    "mod:     {} v{ver} {:?} enabled={}",
                    m.stem, m.state, m.enabled
                );
            }
            Ok(())
        }
        "launch" => game::launch(&paths.game).map_err(|e| e.to_string()),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}
