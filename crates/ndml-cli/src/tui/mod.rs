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

//! The retro terminal UI. `app` holds state/input, `ui` draws, `fx` and
//! `theme` provide the look.

mod app;
mod fx;
mod theme;
mod ui;

use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{SetSize, SetTitle, size};

const FRAME: Duration = Duration::from_millis(66);

/// Render one frame of `tab` (1-4) to plain text at the design size, with the
/// first mod whose name contains `mod_filter` selected and its config focused.
/// Used for docs and layout checks: `ndml snapshot 1 magnet_range`.
pub fn snapshot(game_dir: Option<&Path>, tab: usize, mod_filter: Option<&str>) -> String {
    use ratatui::backend::TestBackend;
    let mut app = app::App::new(game_dir);
    app.tab = app::Tab::ALL[tab.clamp(1, 4) - 1];
    app.notice = std::env::var_os("NDML_SNAPSHOT_NOTICE").is_some();
    if let Some(filter) = mod_filter {
        app.focus_mod_config(filter);
    }
    for _ in 0..30 {
        app.on_tick(theme::WIDTH, theme::HEIGHT);
    }
    let mut terminal = match ratatui::Terminal::new(TestBackend::new(theme::WIDTH, theme::HEIGHT)) {
        Ok(t) => t,
        Err(never) => match never {},
    };
    let _ = terminal.draw(|f| ui::draw(f, &app));
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

pub fn run(game_dir: Option<&Path>) -> std::io::Result<()> {
    let original = size().ok();
    // Ask for the 120x36 design size; terminals that can't resize (e.g.
    // Windows Terminal) ignore this and the UI centres/warns instead.
    let _ = execute!(
        stdout(),
        SetTitle("NDML :: Nova Drift Mod Loader"),
        SetSize(theme::WIDTH, theme::HEIGHT)
    );

    let mut terminal = ratatui::init();
    let mut app = app::App::new(game_dir);
    let result = (|| -> std::io::Result<()> {
        let mut last = Instant::now();
        while !app.quit {
            let area = terminal.size()?;
            terminal.draw(|f| ui::draw(f, &app))?;
            let timeout = FRAME.saturating_sub(last.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        app.on_key(k);
                    }
                }
            }
            if last.elapsed() >= FRAME {
                app.on_tick(area.width, area.height);
                last = Instant::now();
            }
        }
        Ok(())
    })();
    ratatui::restore();
    if let Some((w, h)) = original {
        let _ = execute!(stdout(), SetSize(w, h));
    }
    result
}
