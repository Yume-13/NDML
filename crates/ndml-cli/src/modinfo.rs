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

//! Reading a mod DLL's embedded metadata.
//!
//! Mods carry an `NDML_MOD_INFO` record as plain data (see
//! `ndml_sdk::modinfo`), so the manager reads it from the file's bytes. The DLL
//! is never loaded or executed, and the file is never locked.

use std::path::Path;

pub use ndml_sdk::modinfo::ModMeta;

pub fn read(dll: &Path) -> Result<ModMeta, String> {
    let bytes = std::fs::read(dll).map_err(|e| format!("cannot read: {e}"))?;
    ndml_sdk::modinfo::parse(&bytes).ok_or_else(|| "no NDML_MOD_INFO record (old SDK?)".to_owned())
}
