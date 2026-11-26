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

//! Rendering. Everything is laid out for a 120x36 frame, centred in larger
//! terminals with the starfield filling the margin.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use super::app::{App, Focus, LOG_LEVELS, Tab, Tone};
use super::fx;
use super::picker::{Picker, Row};
use super::theme::{
    self, AMBER, BG, CYAN, CYAN_DIM, DIM, GREEN, HEIGHT, MAGENTA, PANEL, RED, SELECT, TEXT, WIDTH,
};
use crate::config_edit::{Field, Rgb};
use crate::deploy::{LoaderState, ModState};
use crate::game::Running;

/// Rows of a mod's description in the dossier; the rest is read with `?`.
const ABOUT_ROWS: usize = 2;

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
    if let Some(p) = &app.picker {
        draw_picker(f, frame, p);
    }
    if let Some(at) = app.reading {
        draw_reading(f, frame, app, at);
    }
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
    let w = area.width.min(64);
    let h = 8;
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h.min(area.height),
    );
    f.render_widget(Clear, r);
    let msg = Paragraph::new(vec![
        Line::styled(
            format!(
                " This window is {}x{}; NDML needs {WIDTH}x{HEIGHT}.",
                area.width, area.height
            ),
            Style::new().fg(TEXT),
        ),
        Line::styled(
            " Make the window larger, or the font smaller (Ctrl+minus).",
            theme::dim(),
        ),
        Line::styled(
            " To open at this size in Windows Terminal: Settings, Startup,",
            theme::dim(),
        ),
        Line::styled(
            format!(" Launch size: {WIDTH} columns, {HEIGHT} rows."),
            theme::dim(),
        ),
        Line::from(""),
        Line::styled(" [Q] quit", Style::new().fg(MAGENTA)),
    ])
    .block(theme::alert("WINDOW TOO SMALL"));
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
    let blink = (app.tick / 5).is_multiple_of(2);
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
        LoaderState::Unreadable(_) => ("version.dll UNREADABLE", false, true),
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
        match app.running {
            Running::Yes => row("GAME", theme::led(true, false), "RUNNING".into(), GREEN),
            Running::No => row("GAME", theme::led(false, false), "IDLE".into(), DIM),
            Running::Unknown => row("GAME", theme::led(true, true), "UNKNOWN".into(), AMBER),
        },
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
                .map_or_default(|i| format!("v{}", i.version));
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
                // Two rows under a hanging indent; a longer one ends in a
                // pointer to `?`, which shows it whole.
                let width = usize::from(dossier.width).saturating_sub(2 + 11);
                let about = fit(wrap_words(&i.description, width), ABOUT_ROWS, width);
                for (n, row) in about.into_iter().enumerate() {
                    lines.push(Line::from(vec![
                        if n == 0 {
                            label("ABOUT")
                        } else {
                            Span::raw(" ".repeat(11))
                        },
                        Span::styled(row, Style::new().fg(TEXT)),
                    ]));
                }
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
    let inner = usize::from(area.width).saturating_sub(2);
    // The path, and what the file is when it is not simply current.
    let note = doc.note().map_or_default(|n| format!("  · {n}"));
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!(
                " {}",
                ellipsize(
                    &doc.path.display().to_string(),
                    inner.saturating_sub(2 + note.chars().count())
                )
            ),
            Style::new().fg(CYAN_DIM),
        ),
        Span::styled(note, theme::dim()),
    ])];
    // A long config scrolls with the selection: the rows between the path
    // line (and the one under it) and the hint (and the help lines above
    // it: two, or more when the settings leave rows free).
    let fields = doc.fields();
    let rows = usize::from(area.height).saturating_sub(7).max(1);
    let start = app
        .cfg_sel
        .saturating_sub(rows - 1)
        .min(fields.len().saturating_sub(rows));
    lines.push(if fields.len() > rows {
        Line::styled(
            format!(
                " settings {}-{} of {}",
                start + 1,
                (start + rows).min(fields.len()),
                fields.len()
            ),
            theme::dim(),
        )
    } else {
        Line::from("")
    });
    let editing = app.editing.is_some();
    for (i, field) in fields.iter().enumerate().skip(start).take(rows) {
        let selected = focused && i == app.cfg_sel;
        let typed = app.editing.as_deref().filter(|_| selected);
        lines.push(field_row(field, selected, typed, app.tick, inner));
    }
    // The selected field's help (from the mod as built), and the mod's
    // default beside a value of the player's that differs from it; with the
    // list not focused, the config's introduction. Rows the fields leave
    // free go to it; what does not fit is read in full with `?`.
    let width = usize::from(area.width).saturating_sub(4);
    let help_rows = 2 + rows.saturating_sub(fields.len());
    let about = if focused {
        fields
            .get(app.cfg_sel)
            .filter(|_| !editing)
            .map_or_default(|f| {
                let mut t = f.help.clone();
                if let Some(d) = &f.shipped {
                    if !t.is_empty() {
                        t.push(' ');
                    }
                    let _ = write!(t, "[default {d}]");
                }
                wrap_words(&t, width)
            })
    } else {
        reflow(&doc.intro(), width)
    };
    let about = fit(about, help_rows, width);
    for n in 0..help_rows {
        let t = about.get(n).map_or("", String::as_str);
        lines.push(Line::styled(format!(" {t}"), Style::new().fg(CYAN_DIM)));
    }
    let list = fields.get(app.cfg_sel).is_some_and(|f| f.kind == "multi");
    let hint = if editing {
        " Type a value · Enter to save · Esc to cancel"
    } else if focused && list {
        " ↑/↓ setting · Enter choose from the list · D defaults · Esc back"
    } else if focused {
        " ←/→ adjust (Shift ×10) · Enter type value · D defaults · Esc back"
    } else {
        " Tab to edit. Changes are saved instantly and apply on next launch."
    };
    lines.push(Line::styled(hint, theme::dim()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Columns of a setting's key: a longer key is cut, ending in `…`.
const KEY_COLUMN: usize = 28;

/// One setting's row, `inner` columns wide: marker, key, dots, then the
/// value (a choice as a selector switch), its kind and a gauge for numbers.
fn field_row(
    field: &Field,
    selected: bool,
    typed: Option<&str>,
    tick: u64,
    inner: usize,
) -> Line<'static> {
    let editing = typed.is_some();
    let bg = if selected { SELECT } else { PANEL };
    let marker = if selected { "▶" } else { " " };
    let key = clip_end(&field.key, KEY_COLUMN);
    let dots = ".".repeat(KEY_COLUMN.saturating_sub(key.chars().count()));
    let mut spans = vec![
        Span::styled(format!(" {marker} "), Style::new().fg(MAGENTA).bg(bg)),
        Span::styled(key, Style::new().fg(TEXT).bg(bg)),
        Span::styled(format!(" {dots} "), Style::new().fg(theme::FAINT).bg(bg)),
    ];
    if field.kind == "choice" && !editing {
        spans.extend(choice_switch(
            field,
            bg,
            inner.saturating_sub(spans_width(&spans)),
        ));
    } else {
        // The value's room: the row less the kind column and the gauge.
        let gauge = if field.number.is_some() { 17 } else { 0 };
        let room = inner
            .saturating_sub(spans_width(&spans) + 8 + gauge)
            .max(12);
        let value = if editing {
            // The end of what is typed, so the cursor stays in view.
            let blink = (tick / 4).is_multiple_of(2);
            let typed = ellipsize(typed.unwrap_or_default(), room.saturating_sub(1));
            format!("{typed}{}", if blink { "█" } else { " " })
        } else if field.kind == "multi" {
            let labels: Vec<&str> = field.values.iter().map(|v| field.label_of(v)).collect();
            clip_end(&labels.join(", "), room)
        } else {
            ellipsize(&field.value, room)
        };
        let value_style = match field.kind {
            // The mod's default, not in the player's file, or a setting the
            // mod no longer has: unlit.
            _ if field.unset || field.unused => Style::new().fg(DIM),
            "bool" if field.value == "ON" => Style::new().fg(GREEN),
            "bool" => Style::new().fg(RED),
            _ => Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
        };
        spans.push(Span::styled(format!("{value:<12}"), value_style.bg(bg)));
        // "default": the player's file does not set it (see `Field::unset`).
        let kind = match field.kind {
            _ if field.unset => "default",
            "multi" => "list",
            k => k,
        };
        spans.push(Span::styled(
            format!(" {kind:<7}"),
            Style::new().fg(DIM).bg(bg),
        ));
    }
    if let Some(n) = field.number {
        spans.push(Span::styled(" ", Style::new().bg(bg)));
        spans.extend(meter(n));
    }
    Line::from(spans)
}

/// A choice as a selector switch: every option, the current one lit (each
/// with its colour swatch when the options are colours), or, when they do
/// not fit in `room` columns, the current one and its place in the list.
fn choice_switch(field: &Field, bg: Color, room: usize) -> Vec<Span<'static>> {
    let lit = Style::new()
        .fg(PANEL)
        .bg(AMBER)
        .add_modifier(Modifier::BOLD);
    let swatch = |c: Option<Rgb>, style: Style| {
        c.map(|[r, g, b]| Span::styled("■ ", style.fg(Color::Rgb(r, g, b))))
    };
    let mut opts = vec![Span::styled("◂", Style::new().fg(CYAN_DIM).bg(bg))];
    for (n, opt) in field.options.iter().enumerate() {
        if n > 0 {
            opts.push(Span::styled("·", Style::new().fg(theme::FAINT).bg(bg)));
        }
        let style = if *opt == field.value {
            lit
        } else {
            Style::new().fg(DIM).bg(bg)
        };
        opts.push(Span::styled(" ", style));
        let colour = field.meta.get(n).and_then(|m| m.colour);
        opts.extend(swatch(colour, style));
        opts.push(Span::styled(format!("{} ", field.label_of(opt)), style));
    }
    opts.push(Span::styled("▸", Style::new().fg(CYAN_DIM).bg(bg)));
    if field.unset {
        opts.push(Span::styled(" default", Style::new().fg(DIM).bg(bg)));
    }
    if spans_width(&opts) <= room {
        return opts;
    }
    let pos = field.options.iter().position(|o| *o == field.value);
    let place = match pos {
        Some(p) => format!("{}/{}", p + 1, field.options.len()),
        None if field.colour().is_some() => "own colour".to_owned(),
        None => format!("?/{}", field.options.len()),
    };
    let mut out = vec![
        Span::styled("◂", Style::new().fg(CYAN_DIM).bg(bg)),
        Span::styled(" ", lit),
    ];
    out.extend(swatch(field.colour(), lit));
    out.extend([
        Span::styled(format!("{} ", field.label_of(&field.value)), lit),
        Span::styled("▸", Style::new().fg(CYAN_DIM).bg(bg)),
        Span::styled(
            format!("  {place}{}", if field.unset { " default" } else { "" }),
            Style::new().fg(DIM).bg(bg),
        ),
    ]);
    out
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

/// Columns a wrapped log line's later rows are indented by: past the time
/// and level, under the target.
const LOG_INDENT: usize = 18;

fn draw_log(f: &mut Frame<'_>, app: &App, area: Rect) {
    let title = format!(
        "LOG · {}{}",
        if app.log_filter {
            "WARNINGS AND ERRORS · "
        } else {
            ""
        },
        if app.log_scroll == 0 {
            "FOLLOWING"
        } else {
            "SCROLLED"
        }
    );
    let block = theme::panel(&title, true);
    let inner_h = usize::from(area.height.saturating_sub(2));
    // Less one column for the scrollbar.
    let width = usize::from(area.width.saturating_sub(3));
    if app.log.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                "  No log yet. The loader writes ndml/logs/ndml.log while the game runs.",
                theme::dim(),
            ),
            Line::styled("  Press [G] to launch Nova Drift.", Style::new().fg(GREEN)),
        ])
        .block(block);
        f.render_widget(p, area);
        return;
    }
    let shown = app.shown_log();
    if shown.is_empty() {
        let p = Paragraph::new(vec![
            Line::from(""),
            Line::styled(
                "  No warnings or errors in this log.",
                Style::new().fg(GREEN),
            ),
            Line::styled("  Press [W] to show every line.", theme::dim()),
        ])
        .block(block);
        f.render_widget(p, area);
        return;
    }
    // Scrolling counts log lines; each is wrapped under a hanging indent,
    // and the rows that fit above the last one shown are drawn.
    let total = shown.len();
    let end = total.saturating_sub(app.log_scroll);
    let mut rows: Vec<Line<'_>> = Vec::new();
    let mut start = end;
    while start > 0 && rows.len() < inner_h {
        start -= 1;
        let mut wrapped = wrap_spans(&log_line(shown[start]).spans, width, LOG_INDENT);
        wrapped.append(&mut rows);
        rows = wrapped;
    }
    let skip = rows.len().saturating_sub(inner_h);
    let lines: Vec<Line<'_>> = rows.into_iter().skip(skip).collect();
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    // No bar when the whole log is on screen.
    let all = start == 0 && skip == 0 && end == total;
    let len = if all { 0 } else { total.saturating_sub(1) };
    let mut sb = ScrollbarState::new(len).position(end.saturating_sub(1));
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_style(Style::new().fg(MAGENTA))
            .track_style(Style::new().fg(CYAN_DIM)),
        area,
        &mut sb,
    );
}

/// Styled text in rows of `width` columns, broken at spaces where it can
/// be (a word longer than a row is split), later rows indented by
/// `indent` columns.
fn wrap_spans(spans: &[Span<'_>], width: usize, indent: usize) -> Vec<Line<'static>> {
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let lead = if rows.is_empty() {
            0
        } else {
            indent.min(width / 2)
        };
        let room = width.saturating_sub(lead).max(1);
        let mut take = (chars.len() - at).min(room);
        if at + take < chars.len() {
            // Break after the last space in the row, if there is one.
            if let Some(sp) = chars[at..at + take].iter().rposition(|(c, _)| *c == ' ')
                && sp > 0
            {
                take = sp + 1;
            }
        }
        let mut row: Vec<(char, Style)> = vec![(' ', Style::new()); lead];
        row.extend_from_slice(&chars[at..at + take]);
        while row.len() > lead && row.last().is_some_and(|(c, _)| *c == ' ') {
            row.pop();
        }
        rows.push(row);
        at += take;
        while at < chars.len() && chars[at].0 == ' ' {
            at += 1;
        }
    }
    rows.into_iter()
        .map(|row| {
            let mut out: Vec<Span<'static>> = Vec::new();
            for (c, style) in row {
                match out.last_mut() {
                    Some(s) if s.style == style => s.content.to_mut().push(c),
                    _ => out.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(out)
        })
        .collect()
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

    let state = match &app.loader {
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
        LoaderState::Unreadable(e) => Span::styled(
            ellipsize(&format!("▲ unreadable ({e})"), 44),
            Style::new().fg(AMBER),
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
        keyline("ENT", "type a value / open a list"),
        keyline(" ? ", "read help in full"),
        keyline(" X ", "remove deployed mod"),
        keyline(" D ", "reset config to defaults"),
        Line::from(""),
        Line::styled(
            " LOG TAB",
            Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        keyline("↑↓", "scroll · PgUp/PgDn · Home"),
        keyline(" F ", "follow (jump to end)"),
        keyline(" W ", "warnings and errors only"),
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
            "  ndml/logs/            ndml.log for this launch, the last nine dated beside it",
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
    let list = app
        .config
        .as_ref()
        .and_then(|c| c.as_ref().ok())
        .and_then(|d| d.fields().into_iter().nth(app.cfg_sel))
        .is_some_and(|f| f.kind == "multi");
    // A checklist or the reading panel takes every key: only its own.
    let modal: &[(&str, &str)] = if app.picker.is_some() {
        &[
            ("↑↓", "move"),
            ("SPC", "tick"),
            ("ENT", "save"),
            ("ESC", "cancel"),
        ]
    } else if app.reading.is_some() {
        &[("↑↓", "scroll"), ("ESC", "close")]
    } else {
        &[]
    };
    let ctx: &[(&str, &str)] = match (app.tab, app.focus) {
        _ if !modal.is_empty() => modal,
        _ if app.editing.is_some() => &[("ENT", "save"), ("ESC", "cancel")],
        (Tab::Mods, Focus::List) => &[
            ("↑↓", "select"),
            ("SPC", "on/off"),
            ("TAB", "config"),
            ("X", "remove"),
            ("?", "about"),
        ],
        (Tab::Mods, Focus::Config) if list => &[
            ("↑↓", "field"),
            ("ENT", "choose"),
            ("?", "read"),
            ("ESC", "back"),
        ],
        (Tab::Mods, Focus::Config) => &[
            ("↑↓", "field"),
            ("←→", "adjust"),
            ("ENT", "edit"),
            ("?", "read"),
            ("ESC", "back"),
        ],
        (Tab::Log, _) if app.log_filter => &[("↑↓", "scroll"), ("F", "follow"), ("W", "all lines")],
        (Tab::Log, _) => &[("↑↓", "scroll"), ("F", "follow"), ("W", "warnings")],
        (Tab::Loader, _) => &[("↑↓", "select"), ("←→", "change")],
        (Tab::Help, _) => &[],
    };
    let global: &[(&str, &str)] = if modal.is_empty() {
        &[("I", "install"), ("G", "launch"), ("Q", "quit")]
    } else {
        &[]
    };
    for (k, l) in ctx.iter().chain(global) {
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

/// Columns the spans take.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

/// `s` cut to `max` characters, keeping its start (`…` at the end).
fn clip_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// `s` in lines of at most `max` characters, broken at spaces (a word
/// longer than a line is split). Empty text is one empty line.
fn wrap_words(s: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let used = line.chars().count();
            let gap = usize::from(used > 0);
            if used + gap + word.len() <= max {
                if gap == 1 {
                    line.push(' ');
                }
                line.extend(word.iter());
                break;
            }
            if used > 0 {
                out.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than a whole line.
            let rest = word.split_off(max);
            out.push(word.iter().collect());
            word = rest;
        }
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Comment lines rewrapped to `width`: each run of lines is a paragraph
/// (an empty line between paragraphs is kept); a line that starts with a
/// space, `-` or `*` starts a new line, as a list item does.
fn reflow(lines: &[String], width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut para = String::new();
    let flush = |para: &mut String, out: &mut Vec<String>| {
        if !para.is_empty() {
            out.extend(wrap_words(para, width));
            para.clear();
        }
    };
    for l in lines {
        if l.trim().is_empty() {
            flush(&mut para, &mut out);
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        if l.starts_with([' ', '-', '*']) {
            flush(&mut para, &mut out);
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(l.trim());
    }
    flush(&mut para, &mut out);
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// What ends text cut to fit: the rest is read with `?`.
const MORE: &str = "… ? more";

/// `lines` in at most `rows` lines of `width` characters: when there are
/// more, the last one kept ends in [`MORE`].
fn fit(mut lines: Vec<String>, rows: usize, width: usize) -> Vec<String> {
    if lines.len() <= rows || rows == 0 {
        lines.truncate(rows);
        return lines;
    }
    lines.truncate(rows);
    let room = width.saturating_sub(MORE.chars().count() + 1);
    if let Some(last) = lines.last_mut() {
        if last.chars().count() > room {
            let cut: String = last.chars().take(room).collect();
            // At a word's end, when there is one.
            *last = match cut.rfind(' ') {
                Some(i) if i > room / 2 => cut[..i].to_owned(),
                _ => cut,
            };
        }
        if !last.is_empty() {
            last.push(' ');
        }
        last.push_str(MORE);
    }
    lines
}

/// A swatch's colour.
const fn swatch_colour([red, green, blue]: Rgb) -> Color {
    Color::Rgb(red, green, blue)
}

/// The checklist of a list setting, over the frame.
fn draw_picker(f: &mut Frame<'_>, frame: Rect, p: &Picker) {
    // As tall as the whole list (headings included) needs, up to 28 rows;
    // filtering does not change it.
    let groups = p
        .entries
        .windows(2)
        .filter(|pair| pair[0].group != pair[1].group && pair[1].group.is_some())
        .count()
        + usize::from(p.entries.first().is_some_and(|first| first.group.is_some()));
    let full = u16::try_from(p.entries.len() + groups).unwrap_or(u16::MAX);
    let (width, height) = (80, full.saturating_add(6).clamp(12, 28));
    let rect = Rect::new(
        frame.x + (frame.width - width) / 2,
        frame.y + (frame.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let inner = usize::from(width - 2);
    let visible = usize::from(height) - 6;
    let count = format!("{} of {} ticked ", p.ticked(), p.entries.len());
    let filter = if p.filter.is_empty() {
        Span::styled(" Type to filter".to_owned(), theme::dim())
    } else {
        Span::styled(format!(" Filter: {}█", p.filter), Style::new().fg(AMBER))
    };
    let gap = inner.saturating_sub(filter.width() + count.chars().count());
    let mut lines = vec![
        Line::from(vec![
            filter,
            Span::raw(" ".repeat(gap)),
            Span::styled(count, theme::dim()),
        ]),
        Line::from(""),
    ];
    let rows = p.rows();
    let cur = rows
        .iter()
        .position(|row| *row == Row::Entry(p.cursor))
        .unwrap_or(0);
    let start = (cur + 1).saturating_sub(visible);
    if rows.is_empty() {
        lines.push(Line::styled(
            format!("  Nothing matches \"{}\".", p.filter),
            theme::dim(),
        ));
    }
    for row in rows.iter().skip(start).take(visible) {
        lines.push(match *row {
            Row::Group(name) => Line::styled(
                format!("  {name}"),
                Style::new().fg(MAGENTA).add_modifier(Modifier::BOLD),
            ),
            Row::Entry(i) => {
                let entry = &p.entries[i];
                let here = i == p.cursor;
                let bg = if here { SELECT } else { PANEL };
                let on = p.on.get(i).copied().unwrap_or(false);
                let mut spans = vec![
                    Span::styled(
                        if here { "  ▶ " } else { "    " },
                        Style::new().fg(MAGENTA).bg(bg),
                    ),
                    Span::styled(
                        if on { "[x] " } else { "[ ] " },
                        Style::new().fg(if on { GREEN } else { DIM }).bg(bg),
                    ),
                ];
                if let Some(rgb) = entry.colour {
                    spans.push(Span::styled(
                        "■ ",
                        Style::new().fg(swatch_colour(rgb)).bg(bg),
                    ));
                }
                spans.push(Span::styled(
                    entry.label.clone(),
                    Style::new()
                        .fg(if entry.unknown { AMBER } else { TEXT })
                        .bg(bg),
                ));
                if entry.label != entry.value {
                    spans.push(Span::styled(
                        format!("  {}", entry.value),
                        Style::new().fg(theme::FAINT).bg(bg),
                    ));
                }
                if let Some(hint) = &entry.hint {
                    spans.push(Span::styled(
                        format!("  {hint}"),
                        Style::new().fg(DIM).bg(bg),
                    ));
                }
                let used = spans_width(&spans);
                spans.push(Span::styled(
                    " ".repeat(inner.saturating_sub(used)),
                    Style::new().bg(bg),
                ));
                Line::from(spans)
            }
        });
    }
    while lines.len() < visible + 3 {
        lines.push(Line::from(""));
    }
    let place = if rows.len() > visible {
        format!(
            "rows {}-{} of {} ",
            start + 1,
            (start + visible).min(rows.len()),
            rows.len()
        )
    } else {
        String::new()
    };
    let help = " Space ticks · Enter saves · Esc leaves the list as it was";
    let gap = inner.saturating_sub(help.chars().count() + place.chars().count());
    lines.push(Line::from(vec![
        Span::styled(help, theme::dim()),
        Span::raw(" ".repeat(gap)),
        Span::styled(place, theme::dim()),
    ]));
    f.render_widget(
        Paragraph::new(lines).block(theme::panel(&p.key, true)),
        rect,
    );
}

/// Size of the reading panel (`?`), and the rows of text it shows.
const READ_W: u16 = 96;
const READ_H: u16 = 28;
const READ_ROWS: usize = READ_H as usize - 5;

/// The reading panel's title and text: the selected setting's help in
/// full, its value, default and options; or, from the mod list, the mod's
/// description and its config's introduction.
fn reading_text(app: &App) -> (String, Vec<Line<'static>>) {
    let width = usize::from(READ_W) - 6;
    let para = |lines: &mut Vec<Line<'static>>, text: Vec<String>, style: Style| {
        lines.extend(
            text.into_iter()
                .map(|t| Line::styled(format!("  {t}"), style)),
        );
    };
    let heading = |t: &str| {
        Line::styled(
            format!("  {t}"),
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        )
    };
    let doc = app.config.as_ref().and_then(|c| c.as_ref().ok());
    let field = doc
        .filter(|_| app.focus == Focus::Config)
        .and_then(|d| d.fields().into_iter().nth(app.cfg_sel));
    let mut lines = Vec::new();
    if let Some(field) = field {
        para(
            &mut lines,
            reflow(&field.help_lines, width),
            Style::new().fg(TEXT),
        );
        lines.push(Line::from(""));
        let now = if field.kind == "multi" {
            let labels: Vec<&str> = field.values.iter().map(|v| field.label_of(v)).collect();
            if labels.is_empty() {
                "none".to_owned()
            } else {
                labels.join(", ")
            }
        } else {
            field.value.clone()
        };
        let now = if field.unset {
            format!("{now} (the mod's default; your file does not set it)")
        } else {
            now
        };
        para(
            &mut lines,
            wrap_words(&format!("Now: {now}"), width),
            Style::new().fg(AMBER),
        );
        if let Some(d) = &field.shipped {
            para(
                &mut lines,
                wrap_words(&format!("Default: {d}"), width),
                theme::dim(),
            );
        }
        if !field.options.is_empty() || field.any_colour {
            lines.push(Line::from(""));
            lines.push(heading("OPTIONS"));
            let mut group: Option<&str> = None;
            for (i, opt) in field.options.iter().enumerate() {
                let meta = field.meta.get(i);
                let g = meta.and_then(|m| m.group.as_deref());
                if g.is_some() && g != group {
                    lines.push(Line::styled(
                        format!("  {}", g.unwrap_or_default()),
                        Style::new().fg(MAGENTA),
                    ));
                }
                group = g;
                let chosen = if field.kind == "multi" {
                    field.values.contains(opt)
                } else {
                    *opt == field.value
                };
                let mut spans = vec![Span::styled(
                    if chosen { "    ● " } else { "    ○ " },
                    Style::new().fg(if chosen { AMBER } else { DIM }),
                )];
                if let Some([r, g, b]) = meta.and_then(|m| m.colour) {
                    spans.push(Span::styled("■ ", Style::new().fg(Color::Rgb(r, g, b))));
                }
                spans.push(Span::styled(opt.clone(), Style::new().fg(TEXT)));
                if let Some(l) = meta.and_then(|m| m.label.as_deref()) {
                    spans.push(Span::styled(format!("  {l}"), theme::dim()));
                }
                if let Some(h) = meta.and_then(|m| m.hint.as_deref()) {
                    spans.push(Span::styled(format!(" ({h})"), theme::dim()));
                }
                lines.push(Line::from(spans));
            }
            if field.any_colour {
                lines.push(Line::styled(
                    "    or any colour typed as #rrggbb",
                    theme::dim(),
                ));
            }
        }
        return (field.key, lines);
    }
    let Some(m) = app.selected_mod() else {
        return ("ABOUT".into(), lines);
    };
    let title = match &m.meta {
        Ok(i) => {
            para(
                &mut lines,
                wrap_words(&i.description, width),
                Style::new().fg(TEXT),
            );
            format!("{} v{}", i.package, i.version)
        }
        Err(e) => {
            para(&mut lines, wrap_words(e, width), Style::new().fg(RED));
            m.stem.clone()
        }
    };
    let intro = doc.map_or_default(|d| reflow(&d.intro(), width));
    if !intro.is_empty() {
        lines.push(Line::from(""));
        lines.push(heading("SETTINGS"));
        para(&mut lines, intro, Style::new().fg(CYAN_DIM));
    }
    (title, lines)
}

/// The furthest the reading panel scrolls.
pub fn reading_max(app: &App) -> usize {
    reading_text(app).1.len().saturating_sub(READ_ROWS)
}

fn draw_reading(f: &mut Frame<'_>, frame: Rect, app: &App, at: usize) {
    let r = Rect::new(
        frame.x + (frame.width - READ_W) / 2,
        frame.y + (frame.height - READ_H) / 2,
        READ_W,
        READ_H,
    );
    f.render_widget(Clear, r);
    let (title, text) = reading_text(app);
    let total = text.len();
    let at = at.min(total.saturating_sub(READ_ROWS));
    let mut lines = vec![Line::from("")];
    lines.extend(text.into_iter().skip(at).take(READ_ROWS));
    while lines.len() < READ_ROWS + 2 {
        lines.push(Line::from(""));
    }
    let place = if total > READ_ROWS {
        format!(
            "  lines {}-{} of {total} · ↑/↓ PgUp/PgDn scroll · Esc close",
            at + 1,
            (at + READ_ROWS).min(total)
        )
    } else {
        "  Esc close".to_owned()
    };
    lines.push(Line::styled(place, theme::dim()));
    f.render_widget(Paragraph::new(lines).block(theme::panel(&title, true)), r);
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
        assert_eq!(ellipsize(r"X:\Games\Steam\Nova Drift", 10), "…ova Drift");
    }

    /// A config longer than the panel scrolls with the selection; unset
    /// keys say `default`; the selected key's help comes from the shipped
    /// defaults, with the default beside a different value of the player's.
    #[test]
    fn a_long_config_scrolls_and_shows_help() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ui-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "# stale help
key00 = 2.5
",
        )
        .unwrap();
        let defaults = (0..60)
            .map(|i| {
                format!(
                    "# What key {i} does.
key{i:02} = 1.0"
                )
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        let mut app = App::new(None);
        app.running = Running::No;
        app.config = Some(crate::config_edit::ConfigDoc::load(&path, &defaults));
        app.focus = Focus::Config;
        let top = render_text(76, 24, |f| draw_config(f, &app, f.area()));
        assert!(top.contains("key00") && top.contains("2.5"), "{top}");
        assert!(top.contains("What key 0 does. [default 1.0]"), "{top}");
        assert!(!top.contains("stale"), "{top}");
        assert!(top.contains("1.0          default"), "{top}");
        assert!(
            top.contains("settings 1-") && !top.contains("key59"),
            "{top}"
        );
        app.cfg_sel = 59;
        let end = render_text(76, 24, |f| draw_config(f, &app, f.area()));
        assert!(end.contains("key59") && !end.contains("key00"), "{end}");
        assert!(
            end.contains("of 60") && end.contains("What key 59 does."),
            "{end}"
        );
        std::fs::remove_file(path).unwrap();
    }

    /// A choice with more options than the row holds shows the current one
    /// and its place (`3/6`), so the row never runs past the panel; a choice
    /// that fits is drawn whole, as before.
    #[test]
    fn a_choice_row_never_runs_past_the_panel() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ui-choice-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let defaults = "# Detail.\n# options: off, low, medium, high, ultra, extreme\ndetail = \"medium\"\n# Short.\n# options: a, b\nshort = \"b\"\n";
        let mut app = App::new(None);
        app.running = Running::No;
        app.config = Some(crate::config_edit::ConfigDoc::load(&path, defaults));
        app.focus = Focus::Config;
        let text = render_text(76, 12, |f| draw_config(f, &app, f.area()));
        let row = text.lines().find(|l| l.contains("detail")).unwrap();
        assert!(row.contains("◂ medium ▸  3/6 "), "{text}");
        assert!(row.ends_with('║'), "inside the border: {text}");
        let short = text.lines().find(|l| l.contains("short")).unwrap();
        assert!(short.contains("◂ a · b ▸"), "{text}");
        // Wide enough: every option, as before.
        let wide = render_text(110, 12, |f| draw_config(f, &app, f.area()));
        assert!(
            wide.contains(" off · low · medium · high · ultra · extreme ▸"),
            "{wide}"
        );
    }

    /// A frame drawn by `draw` at `w` x `h`, as plain text.
    fn render_text(w: u16, h: u16, draw: impl FnOnce(&mut Frame<'_>)) -> String {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(draw).unwrap();
        let buf = t.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    /// Characters of a description that fit one of the dossier's ABOUT rows at the
    /// launch size: 120 columns, less the mod bay (44), the dossier's
    /// borders (2) and the label (11).
    const ABOUT_WIDTH: usize = 63;

    /// Each mod's description fits the dossier's two ABOUT rows (a longer
    /// one would end in "… ? more" there).
    #[test]
    fn descriptions_fit_the_dossier_rows() {
        let mods = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods");
        let mut seen = 0;
        for entry in std::fs::read_dir(&mods).unwrap() {
            let dir = entry.unwrap().path();
            let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
                continue;
            };
            seen += 1;
            let m = dir.display();
            let d = text
                .lines()
                .find_map(|l| l.strip_prefix("description = \""))
                .and_then(|l| l.strip_suffix('"'))
                .unwrap();
            assert!(wrap_words(d, ABOUT_WIDTH).len() <= ABOUT_ROWS, "{m}: {d}");
        }
        // Every mod in the repository (the probe included).
        assert!(seen >= 3, "{seen} mods found in {}", mods.display());
        let mut t =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(WIDTH, HEIGHT)).unwrap();
        t.draw(|f| {
            let [_, right] =
                Layout::horizontal([Constraint::Length(44), Constraint::Min(0)]).areas(f.area());
            assert_eq!(usize::from(right.width) - 2 - 11, ABOUT_WIDTH);
        })
        .unwrap();
    }

    #[test]
    fn text_wraps_and_is_cut_with_a_pointer_to_the_rest() {
        assert_eq!(wrap_words("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap_words("aaa bbb ccc ddd", 7), ["aaa bbb", "ccc ddd"]);
        assert_eq!(wrap_words("abcdefghij k", 4), ["abcd", "efgh", "ij k"]);
        assert_eq!(wrap_words("", 7), [""]);
        let three = wrap_words("aaaa bbbb cccc dddd eeee ffff", 14);
        assert_eq!(three, ["aaaa bbbb cccc", "dddd eeee ffff"]);
        assert_eq!(fit(three.clone(), 2, 14), three, "fits: unchanged");
        assert_eq!(fit(three, 1, 14), ["aaaa … ? more"]);
        let cut = fit(wrap_words(&"word ".repeat(40), 30), 2, 30);
        assert_eq!(cut.len(), 2);
        assert!(
            cut[1].ends_with(MORE) && cut[1].chars().count() <= 30,
            "{cut:?}"
        );
        assert_eq!(clip_end("spawn_interval", 8), "spawn_i…");
        assert_eq!(clip_end("short", 8), "short");
        let lines: Vec<String> = ["One two", "three.", "", "- a list", "- item", "  kept"]
            .map(String::from)
            .into();
        assert_eq!(
            reflow(&lines, 40),
            ["One two three.", "", "- a list", "- item", "kept"]
        );
    }

    /// A wrapped log line's later rows sit under a hanging indent, broken
    /// at spaces, and a word longer than a row is split.
    #[test]
    fn log_lines_wrap_under_a_hanging_indent() {
        let line = log_line(
            "[    1.204] WARN  ndmod_test: unable to hook gml_Object_oWidgetsPanel_Draw_64 (not found)",
        );
        let rows = wrap_spans(&line.spans, 40, 18);
        let text: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "[    1.204] WARN  ndmod_test: unable to");
        assert!(
            text[1..].iter().all(|t| t.starts_with(&" ".repeat(18))),
            "{text:?}"
        );
        assert!(text.iter().all(|t| t.chars().count() <= 40), "{text:?}");
        let joined: String = text.iter().map(|t| t.trim()).collect::<Vec<_>>().join(" ");
        assert!(joined.contains("gml_Object_oWidget"), "{text:?}");
    }

    /// A key longer than its column is cut with `…` (the row keeps its
    /// layout), and a typed value longer than its room shows its end and
    /// the cursor, inside the panel.
    #[test]
    fn long_keys_and_typed_values_stay_in_their_columns() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ui-long-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let defaults = "# Long.\nspawn_interval_multiplier_late = false\n# Path.\nfolder = \"\"\n";
        let mut app = App::new(None);
        app.running = Running::No;
        app.config = Some(crate::config_edit::ConfigDoc::load(&path, defaults));
        app.focus = Focus::Config;
        let text = render_text(76, 14, |f| draw_config(f, &app, f.area()));
        let row = text.lines().find(|l| l.contains("spawn_interval")).unwrap();
        assert!(row.contains("spawn_interval_multiplier_l…  OFF"), "{text}");
        app.cfg_sel = 1;
        let typed = "C:/Games/Captures/practice runs/a-long-directory/the-end-of-it";
        app.editing = Some(typed.into());
        app.tick = 0;
        let text = render_text(76, 14, |f| draw_config(f, &app, f.area()));
        let row = text.lines().find(|l| l.contains("folder")).unwrap();
        assert!(row.contains("the-end-of-it█"), "{text}");
        assert!(row.contains('…') && row.ends_with('║'), "{text}");
    }

    /// `?` on a setting shows its whole help, its value and its options.
    #[test]
    fn the_reading_panel_has_the_whole_help() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("ui-read-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let long = "word ".repeat(150);
        let defaults =
            format!("# {long}\n# The last line.\n# options: a #ff0000, b, #rrggbb\npick = \"a\"\n");
        let mut app = App::new(None);
        app.running = Running::No;
        app.config = Some(crate::config_edit::ConfigDoc::load(&path, &defaults));
        app.focus = Focus::Config;
        let panel = render_text(76, 14, |f| draw_config(f, &app, f.area()));
        assert!(panel.contains(MORE), "{panel}");
        let (title, lines) = reading_text(&app);
        assert_eq!(title, "pick");
        let text: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("The last line."), "{text}");
        assert!(text.contains("Now: a"), "{text}");
        assert!(text.contains("● ■ a") && text.contains("○ b"), "{text}");
        assert!(text.contains("#rrggbb"), "{text}");
    }

    #[test]
    fn meter_bounds() {
        assert_eq!(meter(0.0).len(), 16);
        assert!(meter(1000.0).iter().all(|s| s.content == "▮"));
    }
}
