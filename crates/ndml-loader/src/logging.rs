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

//! File (and optional console) logger for the loader and all mods.

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

struct Logger {
    start: Instant,
    file: Mutex<Option<File>>,
    console: Mutex<Option<File>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &log::Record<'_>) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let t = self.start.elapsed().as_secs_f64();
        let line = format!("[{t:9.3}] {:<5} {}: {}\n", r.level(), r.target(), r.args());
        for sink in [&self.file, &self.console] {
            if let Some(f) = sink.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Some(f) = self
            .file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            let _ = f.flush();
        }
    }
}

/// Start logging to `path` (truncated each launch).
pub fn init(path: &Path, level: log::LevelFilter, console: bool) {
    let console = console.then(open_console).flatten();
    let logger = LOGGER.get_or_init(|| Logger {
        start: Instant::now(),
        file: Mutex::new(File::create(path).ok()),
        console: Mutex::new(console),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(level);
    }
}

fn open_console() -> Option<File> {
    crate::win::alloc_console();
    std::fs::OpenOptions::new().write(true).open("CONOUT$").ok()
}
