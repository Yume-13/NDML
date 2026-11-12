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

//! Rendering. Everything is laid out for a 120x36 frame, centred in larger
//! terminals with the starfield filling the margin.

use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{App, Focus, LOG_LEVELS, Tab, Tone};
use super::fx;
use super::theme::{
    self, AMBER, BG, CYAN, CYAN_DIM, DIM, GREEN, HEIGHT, MAGENTA, PANEL, RED, SELECT, TEXT, WIDTH,
};
use crate::deploy::{LoaderState, ModState};

pub fn draw(f: &mut Frame<'_>, app: &App) {
    let area = f.area();
    f.render_widget(Block::new().style(Style::new().bg(BG)), area);
    app.stars.render(f.buffer_mut(), area);

    if area.width < WIDTH || area.height < HEIGHT {
        too_small(f, area);
        return;
    }
    let frame = Rect::new(
        area.x + (area.width - WIDTH) / 2,
        area.y + (area.height - HEIGHT) / 2,
        WIDTH,
        HEIGHT,
    );
    f.render_widget(Clear, frame);
    f.render_widget(Block::new().style(Style::new().bg(BG)), frame);

    let [header, tabs, body, status, keys] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame);

    draw_header(f, app, header);
    draw_tabs(f, app, tabs);
    match app.tab {
        Tab::Mods => draw_mods(f, app, body),
        Tab::Log => draw_log(f, app, body),
        Tab::Loader => draw_loader(f, app, body),
        Tab::Help => draw_help(f, body),
    }
    draw_status(f, app, status);
    draw_keys(f, app, keys);
    if let Some(c) = app.confirm {
        draw_confirm(f, frame, c.prompt());
    }
    if app.notice {
        draw_notice(f, frame);
    }
}

fn draw_notice(f: &mut Frame<'_>, frame: Rect) {
    let lines = crate::tui::app::SAFETY_NOTICE;
    let w = 74;
    let h = u16::try_from(lines.len()).unwrap_or(0) + 6;
    let r = Rect::new(
        frame.x + (frame.width - w) / 2,
        frame.y + (frame.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, r);
    let mut text = vec![Line::from("")];
    text.extend(
        lines
            .iter()
            .map(|l| Line::styled(format!("  {l}"), Style::new().fg(TEXT))),
    );
    text.push(Line::from(""));
    let mut keys = vec![Span::raw("  ")];
    keys.extend(theme::key("ANY KEY", "continue"));
    text.push(Line::from(keys));
    f.render_widget(Paragraph::new(text).block(theme::alert("NOTICE")), r);
}

fn too_small(f: &mut Frame<'_>, area: Rect) {
    let w = area.width.min(54);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height.saturating_sub(5) / 2,
        w,
        5.min(area.height),
    );
    f.render_widget(Clear, r);
    let msg = Paragraph::new(vec![
        Line::styled(
            format!(
                "Viewport {}x{} — NDML needs {WIDTH}x{HEIGHT}.",
                area.width, area.height
            ),
            Style::new().fg(TEXT),
        ),
        Line::styled("Enlarge the window or reduce the font size.", theme::dim()),
        Line::styled("[Q] quit", Style::new().fg(MAGENTA)),
    ])
    .block(theme::alert("VIEWPORT TOO SMALL"));
    f.render_widget(msg, r);
}

fn draw_header(f: &mut Frame<'_>, app: &App, area: Rect) {
    let [art, sys] = Layout::horizontal([Constraint::Min(0), Constraint::Length(58)]).areas(area);
    app.stars.render(f.buffer_mut(), art);
    // Keep stars out of the letters' counters so the logo reads cleanly.
    let logo = Rect::new(art.x + 3, art.y + 1, fx::BANNER_WIDTH, 6).intersection(art);
    f.render_widget(Clear, logo);
    f.render_widget(Block::new().style(Style::new().bg(BG)), logo);
    fx::banner(f.buffer_mut(), art.x + 3, art.y + 1, app.tick);

    let tag_x = art.x + 3 + fx::BANNER_WIDTH + 2;
    let tag = Paragraph::new(vec![
        Line::from(Span::styled(
            "NOVA DRIFT",
            Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "MOD LOADER",
            Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            theme::dim(),
        )),
        Line::from(""),
        Line::from(Span::styled("YYC HOOK ENGINE", Style::new().fg(CYAN_DIM))),
    ]);
    if tag_x + 16 <= art.right() {
        f.render_widget(tag, Rect::new(tag_x, art.y + 1, 16, 5));
    }
    let blink = (app.tick / 5) % 2 == 0;
    let cursor = Span::styled(if blink { "█" } else { " " }, Style::new().fg(GREEN));
    let prompt = Line::from(vec![
        Span::styled("  C:\\NDML> ", Style::new().fg(GREEN)),
        cursor,
    ]);
    f.render_widget(
        Paragraph::new(prompt),
        Rect::new(art.x, art.bottom() - 1, art.width, 1),
    );

    let (loader_text, loader_on, loader_warn) = match app.loader {
        LoaderState::Missing => ("NOT INSTALLED", false, false),
        LoaderState::Installed { up_to_date: true } => ("INSTALLED", true, false),
        LoaderState::Installed { up_to_date: false } => ("INSTALLED · UPDATE READY", true, true),
        LoaderState::Foreign => ("FOREIGN version.dll!", false, false),
    };
    let row = |label: &str, led: Span<'static>, value: String, color| {
        Line::from(vec![
            Span::styled(format!(" {label:<8}"), theme::dim()),
            led,
            Span::styled(format!(" {value}"), Style::new().fg(color)),
        ])
    };
    let game = app.paths.as_ref().map_or_else(
        || "NOT FOUND".to_owned(),
        |p| ellipsize(&p.game.display().to_string(), 44),
    );
    let lines = vec![
        row(
            "LOADER",
            theme::led(loader_on, loader_warn),
            loader_text.to_owned(),
            TEXT,
        ),
        row(
            "GAME",
            theme::led(app.running, false),
            if app.running {
                "RUNNING".into()
            } else {
                "IDLE".into()
            },
            if app.running { GREEN } else { DIM },
        ),
        row(
            "MODS",
            theme::led(app.active_mods() > 0, false),
            format!("{} ACTIVE / {} KNOWN", app.active_mods(), app.mods.len()),
            TEXT,
        ),
        row(
            "PATH",
            Span::styled("◆", Style::new().fg(CYAN_DIM)),
            game,
            DIM,
        ),
        row(
            "BUILD",
            Span::styled("◆", Style::new().fg(CYAN_DIM)),
            ellipsize(&app.build.display().to_string(), 44),
            DIM,
        ),
    ];
    f.render_widget(
        Paragraph::new(lines).block(theme::panel("SYSTEM", false)),
        sys,
    );
}

fn draw_tabs(f: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(" ", Style::new())];
    for (i, t) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, t.title());
        if *t == app.tab {
            spans.push(Span::styled("▐", Style::new().fg(MAGENTA)));
            spans.push(Span::styled(
                label,
                Style::new().fg(BG).bg(MAGENTA).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("▌", Style::new().fg(MAGENTA)));
        } else {
            spans.push(Span::styled(format!(" {label} "), Style::new().fg(DIM)));
        }
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let rule = "─".repeat(usize::from(area.width).saturating_sub(used + 1));
    spans.push(Span::styled(rule, Style::new().fg(CYAN_DIM)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn state_badge(state: ModState) -> Span<'static> {
    let (text, color) = match state {
        ModState::Synced => ("● SYNCED", GREEN),
        ModState::Outdated => ("▲ UPDATE", AMBER),
        ModState::NotDeployed => ("○ NOT DEPLOYED", DIM),
        ModState::External => ("◇ EXTERNAL", CYAN),
    };
    Span::styled(text, Style::new().fg(color))
}

fn draw_mods(f: &mut Frame<'_>, app: &App, area: Rect) {
    let [list_area, right] =
        Layout::horizontal([Constraint::Length(44), Constraint::Min(0)]).areas(area);
    let [dossier, config] =
        Layout::vertical([Constraint::Length(10), Constraint::Min(0)]).areas(right);

    // Mod bay. The selection band is painted per span (not via the List's
    // highlight style) so the ON/OFF switch keeps its own colours when selected.
    let row_width = usize::from(list_area.width.saturating_sub(4)); // borders + "▶ "
    let items: Vec<ListItem<'_>> = app
        .mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let band = |style: Style| {
                if i == app.mod_sel {
                    style.bg(SELECT)
                } else {
                    style
                }
            };
            let (sw, sw_style) = if m.enabled {
                (
                    "[ON ]",
                    Style::new().fg(BG).bg(GREEN).add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "[OFF]",
                    Style::new().fg(TEXT).bg(RED).add_modifier(Modifier::BOLD),
                )
            };
            let name = m
                .meta
                .as_ref()
                .map_or(m.stem.as_str(), |i| i.package.as_str());
            let version = m
                .meta
                .as_ref()
                .map(|i| format!("v{}", i.version))
                .unwrap_or_default();
            let lines = vec![
                vec![
                    Span::styled(sw, sw_style),
                    Span::styled(
                        format!(" {name:<24}"),
                        band(Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(version, band(theme::dim())),
                ],
                vec![Span::styled("      ", band(Style::new())), {
                    let badge = state_badge(m.state);
                    let style = band(badge.style);
                    badge.style(style)
                }],
                vec![],
            ];
            let lines = lines
                .into_iter()
                .map(|mut spans| {
                    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                    spans.push(Span::styled(
                        " ".repeat(row_width.saturating_sub(used)),
                        band(Style::new()),
                    ));
                    Line::from(spans)
                })
                .collect::<Vec<_>>();
            ListItem::new(lines)
        })
        .collect();
    let list_focus = app.focus == Focus::List;
    let block = theme::panel("MOD BAY", list_focus);
    if items.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(""),
            Line::styled("  No mods found.", Style::new().fg(AMBER)),
            Line::from(""),
            Line::styled("  Build them:", theme::dim()),
            Line::styled(
                "    ndml.cmd  (or cargo build --release)",
                Style::new().fg(GREEN),
            ),
            Line::styled("  then press [R] to rescan.", theme::dim()),
        ])
        .block(block);
        f.render_widget(empty, list_area);
    } else {
        let list = List::new(items).block(block).highlight_symbol(Line::styled(
            if list_focus { "▶ " } else { "▷ " },
            Style::new().fg(MAGENTA),
        ));
        let mut state = ListState::default().with_selected(Some(app.mod_sel));
        f.render_stateful_widget(list, list_area, &mut state);
    }

    // Dossier
    let mut lines = Vec::new();
    if let Some(m) = app.selected_mod() {
        let label = |l: &str| Span::styled(format!(" {l:<10}"), theme::dim());
        match &m.meta {
            Ok(i) => {
                lines.push(Line::from(vec![
                    label("MOD"),
                    Span::styled(
                        i.package.clone(),
                        Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  v{}", i.version), theme::dim()),
                ]));
                lines.push(Line::from(vec![
                    label("ABOUT"),
                    Span::styled(i.description.clone(), Style::new().fg(TEXT)),
                ]));
            }
            Err(e) => lines.push(Line::from(vec![
                label("MOD"),
                Span::styled(format!("{} ({e})", m.stem), Style::new().fg(RED)),
            ])),
        }
        lines.push(Line::from(vec![label("STATUS"), state_badge(m.state)]));
        lines.push(Line::from(vec![
            label("FILE"),
            Span::styled(format!("{}.dll", m.stem), Style::new().fg(TEXT)),
        ]));
        lines.push(Line::from(vec![
            label("BUILT"),
            Span::styled(age(m.built.as_ref().and_then(|a| a.modified)), theme::dim()),
        ]));
        lines.push(Line::from(vec![
            label("DEPLOYED"),
            Span::styled(
                age(m.deployed.as_ref().and_then(|a| a.modified)),
                theme::dim(),
            ),
        ]));
        if m.enabled && m.deployed.is_none() {
            lines.push(Line::styled(
                " ▲ Press [I] to deploy this mod to the game.",
                Style::new().fg(AMBER),
            ));
        }
    } else {
        lines.push(Line::styled(" Select a mod.", theme::dim()));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(theme::panel("DOSSIER", false)),
        dossier,
    );

    draw_config(f, app, config);
}

fn draw_config(f: &mut Frame<'_>, app: &App, area: Rect) {
    let focused = app.focus == Focus::Config;
    let block = theme::panel("CONFIGURATION", focused);
    let doc = match &app.config {
        Some(Ok(doc)) => doc,
        Some(Err(e)) => {
            f.render_widget(
                Paragraph::new(Line::styled(format!(" {e}"), theme::dim())).block(block),
                area,
            );
            return;
        }
        None => {
            f.render_widget(
                Paragraph::new(Line::styled(" —", theme::dim())).block(block),
                area,
            );
            return;
        }
    };
    let mut lines = vec![Line::styled(
        format!(
            " {}",
            ellipsize(
                &doc.path.display().to_string(),
                usize::from(area.width).saturating_sub(4)
            )
        ),
        Style::new().fg(CYAN_DIM),
    )];
    lines.push(Line::from(""));
    for (i, field) in doc.fields().iter().enumerate() {
        let selected = focused && i == app.cfg_sel;
        let bg = if selected { SELECT } else { PANEL };
        let marker = if selected { "▶" } else { " " };
        let dots = ".".repeat(28usize.saturating_sub(field.key.len()));
        let value = if selected && app.editing.is_some() {
            let blink = (app.tick / 4) % 2 == 0;
            format!(
                "{}{}",
                app.editing.as_deref().unwrap_or(""),
                if blink { "█" } else { " " }
            )
        } else {
            field.value.clone()
        };
        let value_style = match field.kind {
            "bool" if field.value == "ON" => Style::new().fg(GREEN),
            "bool" => Style::new().fg(RED),
            _ => Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        };
        let mut spans = vec![
            Span::styled(format!(" {marker} "), Style::new().fg(MAGENTA).bg(bg)),
            Span::styled(field.key.clone(), Style::new().fg(TEXT).bg(bg)),
            Span::styled(format!(" {dots} "), Style::new().fg(theme::FAINT).bg(bg)),
        ];
        if field.kind == "choice" && !(selected && app.editing.is_some()) {
            // Every option, the current one lit like a selector switch.
            spans.push(Span::styled("◂", Style::new().fg(CYAN_DIM).bg(bg)));
            for (n, opt) in field.options.iter().enumerate() {
                if n > 0 {
                    spans.push(Span::styled("·", Style::new().fg(theme::FAINT).bg(bg)));
                }
                let style = if *opt == field.value {
                    Style::new()
                        .fg(PANEL)
                        .bg(AMBER)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(DIM).bg(bg)
                };
                spans.push(Span::styled(format!(" {opt} "), style));
            }
            spans.push(Span::styled("▸", Style::new().fg(CYAN_DIM).bg(bg)));
        } else {
            spans.push(Span::styled(format!("{value:<12}"), value_style.bg(bg)));
            spans.push(Span::styled(
                format!(" {:<6}", field.kind),
                Style::new().fg(DIM).bg(bg),
            ));
        }
        if let Some(n) = field.number {
            spans.push(Span::styled(" ", Style::new().bg(bg)));
            spans.extend(meter(n));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    let hint = if app.editing.is_some() {
        " Type a value · Enter to save · Esc to cancel"
    } else if focused {
        " ←/→ adjust (Shift ×10) · Enter type value · D defaults · Esc back"
    } else {
        " Tab to edit. Changes are saved instantly and apply on next launch."
    };
    lines.push(Line::styled(hint, theme::dim()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// A 16-segment log-ish meter for numeric values (1.0 lights ~1/3).
fn meter(v: f64) -> Vec<Span<'static>> {
    const SEGMENTS: usize = 16;
    let lit = ((v.max(0.0) + 1.0).log2() / 5.0 * SEGMENTS as f64)
        .round()
        .clamp(0.0, SEGMENTS as f64) as usize;
    (0..SEGMENTS)
        .map(|i| {
            let t = i as f32 / SEGMENTS as f32;
            let color = if i < lit {
                theme::lerp(GREEN, MAGENTA, t)
            } else {
                theme::FAINT
            };
            Span::styled(if i < lit { "▮" } else { "▯" }, Style::new().fg(color))
        })
        .collect()
}

fn log_line(raw: &str) -> Line<'_> {
    // Format: "[  12.345] LEVEL target: message"
    let Some((time, rest)) = raw.split_once("] ") else {
        return Line::styled(raw, Style::new().fg(TEXT));
    };
    let (level, rest) = rest.split_at(rest.len().min(5));
    let color = match level.trim() {
        "ERROR" => RED,
        "WARN" => AMBER,
        "INFO" => CYAN,
        "DEBUG" => DIM,
        _ => theme::FAINT,
    };
    let (target, msg) = rest.split_once(": ").unwrap_or(("", rest));
    Line::from(vec![
        Span::styled(format!("{time}] "), Style::new().fg(theme::FAINT)),
        Span::styled(
            level.to_owned(),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{target}: "), Style::new().fg(MAGENTA)),
        Span::styled(
            msg.to_owned(),
            Style::new().fg(if color == RED || color == AMBER {
                color
            } else {
                TEXT
            }),
        ),
    ])
}

fn draw_log(f: &mut Frame<'_>, app: &App, area: Rect) {
    let title = if app.log_scroll == 0 {
        "LOG · FOLLOWING"
    } else {
        "LOG · SCROLLED"
    };
    let block = theme::panel(title, true);
    let inner_h = usize::from(area.height.saturating_sub(2));
    if app.log.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                "  No log yet. The loader writes ndml/logs/ndml.log each time the game starts.",
                theme::dim(),
            ),
            Line::styled("  Press [G] to launch Nova Drift.", Style::new().fg(GREEN)),
        ])
        .block(block);
        f.render_widget(p, area);
        return;
    }
    let total = app.log.len();
    let end = total.saturating_sub(app.log_scroll);
    let start = end.saturating_sub(inner_h);
    let lines: Vec<Line<'_>> = app.log[start..end].iter().map(|l| log_line(l)).collect();
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    let mut sb = ScrollbarState::new(total.saturating_sub(inner_h)).position(start);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_style(Style::new().fg(MAGENTA))
            .track_style(Style::new().fg(CYAN_DIM)),
        area,
        &mut sb,
    );
}

fn draw_loader(f: &mut Frame<'_>, app: &App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(58), Constraint::Min(0)]).areas(area);

    let setting = |i: usize, key: &str, value: Line<'static>| {
        let selected = app.loader_sel == i;
        let bg = if selected { SELECT } else { PANEL };
        let mut spans = vec![
            Span::styled(
                if selected { " ▶ " } else { "   " },
                Style::new().fg(MAGENTA).bg(bg),
            ),
            Span::styled(format!("{key:<14}"), Style::new().fg(TEXT).bg(bg)),
        ];
        spans.extend(value.spans);
        Line::from(spans)
    };
    let levels: Vec<Span<'static>> = LOG_LEVELS
        .iter()
        .map(|l| {
            if *l == app.settings.log_level {
                Span::styled(
                    format!(" {} ", l.to_uppercase()),
                    Style::new().fg(BG).bg(CYAN).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {l} "), theme::dim())
            }
        })
        .collect();
    let console = if app.settings.console {
        Span::styled(
            " ON  ",
            Style::new().fg(BG).bg(GREEN).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(" OFF ", Style::new().fg(TEXT).bg(RED))
    };
    let disabled = if app.settings.disabled_mods.is_empty() {
        "none".to_owned()
    } else {
        app.settings.disabled_mods.join(", ")
    };
    let lines = vec![
        Line::from(""),
        setting(0, "LOG LEVEL", Line::from(levels)),
        Line::from(""),
        setting(
            1,
            "CONSOLE",
            Line::from(vec![
                console,
                Span::styled("  live log window", theme::dim()),
            ]),
        ),
        Line::from(""),
        Line::from(vec![
            Span::styled("   DISABLED      ", Style::new().fg(TEXT)),
            Span::styled(disabled, theme::dim()),
        ]),
        Line::from(""),
        Line::styled(
            "   ↑/↓ select · ←/→ change · saves ndml/ndml.toml",
            theme::dim(),
        ),
    ];
    f.render_widget(
        Paragraph::new(lines).block(theme::panel("LOADER SETTINGS", true)),
        left,
    );

    let state = match app.loader {
        LoaderState::Missing => Span::styled("○ not installed", Style::new().fg(DIM)),
        LoaderState::Installed { up_to_date: true } => {
            Span::styled("● installed, up to date", Style::new().fg(GREEN))
        }
        LoaderState::Installed { up_to_date: false } => {
            Span::styled("▲ installed, newer build available", Style::new().fg(AMBER))
        }
        LoaderState::Foreign => Span::styled(
            "✖ version.dll belongs to something else",
            Style::new().fg(RED),
        ),
    };
    let built = crate::deploy::built_loader(&app.build);
    let path = |p: String| Span::styled(ellipsize(&p, 44), theme::dim());
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  version.dll  ", Style::new().fg(TEXT)),
            state,
        ]),
        Line::from(vec![
            Span::styled("  game folder  ", Style::new().fg(TEXT)),
            path(
                app.paths
                    .as_ref()
                    .map_or_else(|| "not found".into(), |p| p.game.display().to_string()),
            ),
        ]),
        Line::from(vec![
            Span::styled("  loader build ", Style::new().fg(TEXT)),
            match &built {
                Some(a) => Span::styled(
                    format!("ndml_loader.dll  {}", age(a.modified)),
                    theme::dim(),
                ),
                None => Span::styled("missing — run ndml.cmd", Style::new().fg(RED)),
            },
        ]),
        Line::from(vec![
            Span::styled("  build dir    ", Style::new().fg(TEXT)),
            path(app.build.display().to_string()),
        ]),
        Line::from(""),
    ];
    if app.needs_deploy() {
        lines.push(Line::styled(
            "  ▲ Changes waiting to be deployed — press [I].",
            Style::new().fg(AMBER),
        ));
    } else {
        lines.push(Line::styled(
            "  ● Everything deployed.",
            Style::new().fg(GREEN),
        ));
    }
    lines.push(Line::from(""));
    for (k, label) in [
        ("I", "install / update loader + all built mods"),
        ("U", "uninstall loader (mods & configs kept)"),
        ("G", "launch Nova Drift"),
        ("R", "rescan"),
    ] {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(theme::key(k, label));
        lines.push(Line::from(spans));
        lines.push(Line::from(""));
    }
    f.render_widget(
        Paragraph::new(lines).block(theme::panel("DEPLOYMENT", false)),
        right,
    );
}

fn draw_help(f: &mut Frame<'_>, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Length(42), Constraint::Min(0)]).areas(area);
    let keyline = |k: &'static str, d: &'static str| {
        let mut spans = vec![Span::raw(" ")];
        spans.extend(theme::key(k, d));
        Line::from(spans)
    };
    let keys = vec![
        Line::styled(
            " GLOBAL",
            Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        keyline("1-4", "switch tab"),
        keyline(" I ", "install / update"),
        keyline(" U ", "uninstall loader"),
        keyline(" G ", "launch game"),
        keyline(" R ", "rescan"),
        keyline(" Q ", "quit"),
        Line::from(""),
        Line::styled(
            " MODS TAB",
            Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        keyline("↑↓", "select mod / field"),
        keyline("SPC", "enable / disable mod"),
        keyline("TAB", "switch list ↔ config"),
        keyline("←→", "adjust value (Shift ×10)"),
        keyline("ENT", "type a value"),
        keyline(" X ", "remove deployed mod"),
        keyline(" D ", "reset config to defaults"),
        Line::from(""),
        Line::styled(
            " LOG TAB",
            Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        keyline("↑↓", "scroll · PgUp/PgDn · Home"),
        keyline(" F ", "follow (jump to end)"),
    ];
    f.render_widget(
        Paragraph::new(keys).block(theme::panel("CONTROLS", false)),
        left,
    );

    let body = vec![
        Line::from(""),
        Line::styled(
            " HOW IT WORKS",
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            " NDML installs as version.dll in the game folder. Windows loads it",
            Style::new().fg(TEXT),
        ),
        Line::styled(
            " before the game; it finds the compiled GML functions by name,",
            Style::new().fg(TEXT),
        ),
        Line::styled(
            " loads ndml/mods/ndmod_*.dll, and lets them hook game code.",
            Style::new().fg(TEXT),
        ),
        Line::from(""),
        Line::styled(
            " WHERE THINGS LIVE (all inside the game folder)",
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Line::styled("  ndml/ndml.toml        loader settings", theme::dim()),
        Line::styled(
            "  ndml/mods/            mod DLLs (load alphabetically)",
            theme::dim(),
        ),
        Line::styled("  ndml/config/          per-mod settings", theme::dim()),
        Line::styled(
            "  ndml/logs/ndml.log    rewritten every launch",
            theme::dim(),
        ),
        Line::from(""),
        Line::styled(
            " BUILDING MODS",
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "  double-click ndml.cmd (rebuilds, then opens this UI)",
            Style::new().fg(GREEN),
        ),
        Line::styled("  Guide: docs/mod-api.md", theme::dim()),
        Line::from(""),
        Line::styled(
            " UNINSTALLING",
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "  [U] removes version.dll; delete ndml/ to remove everything.",
            theme::dim(),
        ),
    ];
    f.render_widget(
        Paragraph::new(body).block(theme::panel("ABOUT", false)),
        right,
    );
}

fn draw_status(f: &mut Frame<'_>, app: &App, area: Rect) {
    let Some((msg, tone, at)) = &app.status else {
        return;
    };
    if at.elapsed() > Duration::from_secs(8) {
        return;
    }
    let color = match tone {
        Tone::Info => CYAN,
        Tone::Good => GREEN,
        Tone::Warn => AMBER,
        Tone::Bad => RED,
    };
    let line = Line::from(vec![
        Span::styled(" » ", Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(msg.clone(), Style::new().fg(color)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keys(f: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![Span::raw(" ")];
    let ctx: &[(&str, &str)] = match (app.tab, app.focus) {
        _ if app.editing.is_some() => &[("ENT", "save"), ("ESC", "cancel")],
        (Tab::Mods, Focus::List) => &[
            ("↑↓", "select"),
            ("SPC", "on/off"),
            ("TAB", "config"),
            ("X", "remove"),
        ],
        (Tab::Mods, Focus::Config) => &[
            ("↑↓", "field"),
            ("←→", "adjust"),
            ("ENT", "edit"),
            ("ESC", "back"),
        ],
        (Tab::Log, _) => &[("↑↓", "scroll"), ("F", "follow")],
        (Tab::Loader, _) => &[("↑↓", "select"), ("←→", "change")],
        (Tab::Help, _) => &[],
    };
    for (k, l) in ctx
        .iter()
        .chain(&[("I", "install"), ("G", "launch"), ("Q", "quit")])
    {
        spans.extend(theme::key(k, l));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_confirm(f: &mut Frame<'_>, frame: Rect, prompt: &str) {
    let w = 64;
    let r = Rect::new(
        frame.x + (frame.width - w) / 2,
        frame.y + frame.height / 2 - 3,
        w,
        6,
    );
    f.render_widget(Clear, r);
    let mut keys = vec![Span::raw("  ")];
    keys.extend(theme::key("Y", "confirm"));
    keys.extend(theme::key("N", "cancel"));
    let p = Paragraph::new(vec![
        Line::from(""),
        Line::styled(format!("  {prompt}"), Style::new().fg(TEXT)),
        Line::from(""),
        Line::from(keys),
    ])
    .block(theme::alert("CONFIRM"));
    f.render_widget(p, r);
}

fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

fn age(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".into() };
    let secs = SystemTime::now()
        .duration_since(t)
        .unwrap_or_default()
        .as_secs();
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsize_keeps_tail() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize(r"D:\SteamLibrary\Nova Drift", 10), "…ova Drift");
    }

    #[test]
    fn meter_bounds() {
        assert_eq!(meter(0.0).len(), 16);
        assert!(meter(1000.0).iter().all(|s| s.content == "▮"));
    }
}
