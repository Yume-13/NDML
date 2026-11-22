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

//! Keyboard key names → Windows virtual-key codes, for config files
//! (`menu_key = "F5"`). Query a key inside a hook with [`crate::Call::key_down`].

#![forbid(unsafe_code)]

/// Virtual-key code for a key name: `A`–`Z`, `0`–`9`, `F1`–`F24`, and
/// `Space`, `Tab`, `Enter`, `Escape`/`Esc`, `Backspace`, `Shift`, `Ctrl`/`Control`,
/// `Alt`, `Insert`, `Delete`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`,
/// `Left`, `Right`, `Numpad0`–`Numpad9`. Case-insensitive. `None` if unknown.
#[must_use]
pub fn vk_code(name: &str) -> Option<u32> {
    let n = name.trim().to_ascii_uppercase();
    let mut chars = n.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && (c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Some(u32::from(c));
    }
    if let Some(num) = n.strip_prefix('F').and_then(|d| d.parse::<u32>().ok()) {
        return (1..=24).contains(&num).then(|| 0x6F + num);
    }
    if let Some(num) = n.strip_prefix("NUMPAD").and_then(|d| d.parse::<u32>().ok()) {
        return (num <= 9).then(|| 0x60 + num);
    }
    Some(match n.as_str() {
        "BACKSPACE" => 0x08,
        "TAB" => 0x09,
        "ENTER" | "RETURN" => 0x0D,
        "SHIFT" => 0x10,
        "CTRL" | "CONTROL" => 0x11,
        "ALT" => 0x12,
        "ESC" | "ESCAPE" => 0x1B,
        "SPACE" => 0x20,
        "PAGEUP" => 0x21,
        "PAGEDOWN" => 0x22,
        "END" => 0x23,
        "HOME" => 0x24,
        "LEFT" => 0x25,
        "UP" => 0x26,
        "RIGHT" => 0x27,
        "DOWN" => 0x28,
        "INSERT" => 0x2D,
        "DELETE" => 0x2E,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names() {
        assert_eq!(vk_code("X"), Some(0x58));
        assert_eq!(vk_code("x"), Some(0x58));
        assert_eq!(vk_code("7"), Some(0x37));
        assert_eq!(vk_code("F1"), Some(0x70));
        assert_eq!(vk_code("f12"), Some(0x7B));
        assert_eq!(vk_code("Space"), Some(0x20));
        assert_eq!(vk_code("Numpad5"), Some(0x65));
        assert_eq!(vk_code("F25"), None);
        assert_eq!(vk_code(""), None);
        assert_eq!(vk_code("Banana"), None);
    }
}
