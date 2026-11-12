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

//! Decorative effects: drifting parallax starfield and the gradient banner.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::theme::{self, CYAN, MAGENTA};

/// "NDML" in the ANSI Shadow figlet font.
const BANNER: [&str; 6] = [
    "███╗   ██╗██████╗ ███╗   ███╗██╗     ",
    "████╗  ██║██╔══██╗████╗ ████║██║     ",
    "██╔██╗ ██║██║  ██║██╔████╔██║██║     ",
    "██║╚██╗██║██║  ██║██║╚██╔╝██║██║     ",
    "██║ ╚████║██████╔╝██║ ╚═╝ ██║███████╗",
    "╚═╝  ╚═══╝╚═════╝ ╚═╝     ╚═╝╚══════╝",
];

pub const BANNER_WIDTH: u16 = 37;

/// Draw the banner at `(x, y)`: cyan→magenta gradient with a shine that sweeps
/// across every few seconds. Spaces are skipped so stars show through.
pub fn banner(buf: &mut Buffer, x: u16, y: u16, tick: u64) {
    let sweep = (tick % 60) as f32 * 1.4 - 10.0; // column of the shine band
    for (row, line) in BANNER.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let t = col as f32 / f32::from(BANNER_WIDTH);
            let mut color = theme::lerp(CYAN, MAGENTA, t);
            let glow = 1.0 - ((col as f32 - row as f32 - sweep).abs() / 3.0).min(1.0);
            color = theme::lerp(color, Color::Rgb(255, 255, 255), glow * 0.8);
            if ch != '█' {
                color = theme::lerp(color, theme::BG, 0.45); // shadow strokes
            }
            let pos = (x + col as u16, y + row as u16);
            if let Some(cell) = buf.cell_mut(pos) {
                cell.set_char(ch)
                    .set_style(Style::new().fg(color).add_modifier(Modifier::BOLD));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Star {
    x: f32,
    y: u16,
    layer: u8,
}

/// Stars drift right-to-left at three parallax speeds.
#[derive(Debug)]
pub struct Starfield {
    stars: Vec<Star>,
    rng: u64,
    width: u16,
    height: u16,
}

impl Starfield {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            stars: Vec::new(),
            rng: seed | 1,
            width: 0,
            height: 0,
        }
    }

    fn next(&mut self) -> u64 {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn spawn(&mut self, x: Option<f32>) -> Star {
        let w = u64::from(self.width.max(1));
        let h = u64::from(self.height.max(1));
        let x = x.unwrap_or((self.next() % w) as f32);
        let y = (self.next() % h) as u16;
        let layer = (self.next() % 7) as u8;
        Star {
            x,
            y,
            layer: match layer {
                0 => 2,
                1 | 2 => 1,
                _ => 0,
            },
        }
    }

    /// Advance one frame; reseeds when the terminal is resized.
    pub fn step(&mut self, width: u16, height: u16) {
        if (width, height) != (self.width, self.height) {
            self.width = width;
            self.height = height;
            let count = usize::from(width) * usize::from(height) / 28;
            self.stars = (0..count).map(|_| self.spawn(None)).collect();
        }
        for i in 0..self.stars.len() {
            let speed = [0.15, 0.45, 1.1][usize::from(self.stars[i].layer)];
            self.stars[i].x -= speed;
            if self.stars[i].x < 0.0 {
                self.stars[i] = self.spawn(Some(f32::from(self.width) - 1.0));
            }
        }
    }

    /// Paint stars into blank cells of `area`.
    pub fn render(&self, buf: &mut Buffer, area: Rect) {
        for s in &self.stars {
            let pos = (s.x as u16, s.y);
            if !area.contains(pos.into()) {
                continue;
            }
            let (ch, color) = match s.layer {
                0 => ('·', theme::FAINT),
                1 => ('∙', theme::DIM),
                _ => ('✦', Color::Rgb(170, 220, 255)),
            };
            if let Some(cell) = buf.cell_mut(pos) {
                if cell.symbol() == " " {
                    cell.set_char(ch).set_fg(color);
                }
            }
        }
    }
}
