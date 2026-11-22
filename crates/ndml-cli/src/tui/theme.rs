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

//! Neon-on-navy palette and shared widget styles.

use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};

pub const BG: Color = Color::Rgb(6, 8, 20);
pub const PANEL: Color = Color::Rgb(10, 14, 32);
pub const SELECT: Color = Color::Rgb(44, 18, 74);
pub const CYAN: Color = Color::Rgb(0, 229, 255);
pub const CYAN_DIM: Color = Color::Rgb(24, 84, 112);
pub const MAGENTA: Color = Color::Rgb(255, 62, 200);
pub const AMBER: Color = Color::Rgb(255, 176, 0);
pub const GREEN: Color = Color::Rgb(57, 255, 20);
pub const RED: Color = Color::Rgb(255, 51, 85);
pub const TEXT: Color = Color::Rgb(200, 214, 255);
pub const DIM: Color = Color::Rgb(90, 106, 138);
pub const FAINT: Color = Color::Rgb(46, 56, 84);

/// Design size: 50% larger than the classic 80x24 terminal.
pub const WIDTH: u16 = 120;
pub const HEIGHT: u16 = 36;

#[must_use]
pub fn dim() -> Style {
    Style::new().fg(DIM)
}

/// A titled panel; focused panels get a bright double border.
#[must_use]
pub fn panel(title: &str, focused: bool) -> Block<'static> {
    let (border_style, border_type, title_style) = if focused {
        (
            Style::new().fg(CYAN),
            BorderType::Double,
            Style::new().fg(BG).bg(CYAN).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Style::new().fg(CYAN_DIM),
            BorderType::Plain,
            Style::new().fg(CYAN).add_modifier(Modifier::BOLD),
        )
    };
    Block::new()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(border_style)
        .style(Style::new().bg(PANEL))
        .title(Line::from(vec![
            Span::styled(
                "▓▒░",
                Style::new().fg(if focused { CYAN } else { CYAN_DIM }),
            ),
            Span::styled(format!(" {title} "), title_style),
            Span::styled(
                "░▒▓",
                Style::new().fg(if focused { CYAN } else { CYAN_DIM }),
            ),
        ]))
}

/// Amber double-bordered box for confirmations.
#[must_use]
pub fn alert(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_set(border::DOUBLE)
        .border_style(Style::new().fg(AMBER))
        .style(Style::new().bg(Color::Rgb(26, 16, 4)))
        .title(Span::styled(
            format!(" ⚠ {title} "),
            Style::new().fg(BG).bg(AMBER).add_modifier(Modifier::BOLD),
        ))
}

/// A keycap like ` I ` followed by its label.
#[must_use]
pub fn key<'a>(cap: &'a str, label: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!(" {cap} "),
            Style::new().fg(BG).bg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}  "), Style::new().fg(DIM)),
    ]
}

/// Status LED `●`: amber when `warn`, otherwise green (on) or red (off).
#[must_use]
pub fn led(on: bool, warn: bool) -> Span<'static> {
    let color = if warn {
        AMBER
    } else if on {
        GREEN
    } else {
        RED
    };
    Span::styled("●", Style::new().fg(color))
}

/// Linear blend between two RGB colours.
#[must_use]
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (a, b) else {
        return a;
    };
    let mix = |x: u8, y: u8| {
        (f32::from(x) + (f32::from(y) - f32::from(x)) * t.clamp(0.0, 1.0)).round() as u8
    };
    Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
}
