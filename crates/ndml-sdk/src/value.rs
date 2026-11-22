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

//! [`Value`], an owned Rust copy of a GML value, for passing arguments to and
//! reading results from scripts, builtin functions and variables.
//!
//! Converting to the game's `RValue` (and back, for strings) needs the loader,
//! so that part lives in `ffi`; this module is the pure data side.

#![forbid(unsafe_code)]

use core::fmt;

use crate::abi::{self, RValue, kind};

/// An owned GML value. Strings are copied out of the game, so a `Value` never
/// borrows game memory and can be kept, sent to other threads, or compared.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `undefined` (also used for "never assigned").
    Undefined,
    /// A GML number (`f64`).
    Real(f64),
    /// A GML `int32`/`int64` (sent to the game as `int64`).
    Int(i64),
    /// A GML boolean (`true`/`false`).
    Bool(bool),
    /// A GML string (copied; invalid UTF-8 is replaced with U+FFFD).
    Str(String),
    /// A GML ref (kind 15) with its raw payload: `OBJECT_REF_TAG | index` for an
    /// object (see [`Value::object`]); other ref kinds (instances, sprites, ...)
    /// round-trip unchanged.
    Ref(u64),
    /// A raw pointer value (kind 3), opaque.
    Ptr(u64),
    /// A value the SDK doesn't decode (array, struct, method, ...). Only its
    /// kind tag is kept; the game's copy is released after a call.
    Other {
        /// The `RValue` type tag (low 24 bits of `kind`).
        kind: u32,
    },
}

impl Value {
    /// A ref to the object with index `index` (as GML's `oPlayer` evaluates to).
    #[must_use]
    pub const fn object(index: u32) -> Self {
        Self::Ref(abi::OBJECT_REF_TAG | index as u64)
    }

    /// The value as a number, for `Real`, `Int` and `Bool`.
    ///
    /// `None` for an `Int` above 2^53 in magnitude: a double cannot hold it
    /// exactly, and such values are handles or references (ids from extension
    /// functions, script and method references near 2^56), where a rounded number names a
    /// different thing. Read those exactly with [`Value::as_i64`].
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::Real(v) => Some(v),
            Self::Int(v) => abi::int_as_f64(v),
            Self::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// The exact integer of an `Int` (`None` for any other kind), for
    /// handles and references that [`Value::as_f64`] cannot hold.
    #[must_use]
    pub const fn as_i64(&self) -> Option<i64> {
        match *self {
            Self::Int(v) => Some(v),
            _ => None,
        }
    }

    /// The text of a `Str`.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// GML truthiness of a number (`> 0.5`), or the boolean itself.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(b) => Some(b),
            // `> 0.5` on an integer, for every magnitude.
            Self::Int(v) => Some(v > 0),
            _ => self.as_f64().map(|v| v > 0.5),
        }
    }

    #[must_use]
    pub fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }

    /// The `RValue` for every kind except `Str`, which needs the game's string
    /// routine (`None` then).
    #[must_use]
    pub(crate) fn to_plain_rvalue(&self) -> Option<RValue> {
        Some(match *self {
            // An undecoded value can't be rebuilt; it is passed as `undefined`.
            Self::Undefined | Self::Other { .. } => RValue::undefined(),
            Self::Real(v) => RValue::real(v),
            Self::Int(v) => RValue::int64(v),
            Self::Bool(b) => RValue::boolean(b),
            Self::Ref(p) => RValue::reference(p),
            Self::Ptr(p) => RValue {
                payload: p,
                flags: 0,
                kind: kind::PTR,
            },
            Self::Str(_) => return None,
        })
    }

    /// Decode every kind except strings (`None` for a string).
    #[must_use]
    pub(crate) fn from_plain_rvalue(v: &RValue) -> Option<Self> {
        Some(match v.type_tag() {
            kind::REAL => Self::Real(f64::from_bits(v.payload)),
            kind::INT32 => Self::Int(i64::from(v.payload as u32 as i32)),
            kind::INT64 => Self::Int(v.payload as i64),
            kind::BOOL => Self::Bool(v.payload != 0), // double 1.0 or integer 1
            kind::UNDEFINED | kind::UNSET => Self::Undefined,
            kind::REF => Self::Ref(v.payload),
            kind::PTR => Self::Ptr(v.payload),
            kind::STRING => return None,
            other => Self::Other { kind: other },
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined => f.write_str("undefined"),
            Self::Real(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Str(s) => write!(f, "{s:?}"),
            Self::Ref(p) => write!(f, "<ref {p:#x}>"),
            Self::Ptr(p) => write!(f, "<ptr {p:#x}>"),
            Self::Other { kind } => write!(f, "<kind {kind}>"),
        }
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Real(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Self::Real(f64::from(v))
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Real(f64::from(v))
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_round_trip() {
        for v in [
            Value::Undefined,
            Value::Real(2.5),
            Value::Int(-3),
            Value::Bool(true),
            Value::object(0x25b),
            Value::Ptr(0x1234),
        ] {
            let rv = v.to_plain_rvalue().unwrap();
            assert!(!rv.is_owning());
            assert_eq!(Value::from_plain_rvalue(&rv), Some(v));
        }
        assert!(Value::from("x").to_plain_rvalue().is_none());
    }

    /// An `Int` is a number up to 2^53 in magnitude; past it `as_f64` gives
    /// `None` and `as_i64` the exact value, a tagged method reference (about 2^56)
    /// included. Truthiness is unchanged.
    #[test]
    fn int_beyond_2_pow_53_is_not_a_number() {
        let edge = 1_i64 << 53;
        assert_eq!(Value::Int(edge).as_f64(), Some(9_007_199_254_740_992.0));
        assert_eq!(Value::Int(-edge).as_f64(), Some(-9_007_199_254_740_992.0));
        assert_eq!(Value::Int(edge + 1).as_f64(), None);
        assert_eq!(Value::Int(-edge - 1).as_f64(), None);
        let method = 72_057_615_512_867_079_i64;
        assert_eq!(Value::Int(method).as_f64(), None);
        assert_eq!(Value::Int(method).as_i64(), Some(method));
        assert_eq!(Value::Int(method).as_bool(), Some(true));
        assert_eq!(Value::Int(i64::MIN).as_bool(), Some(false));
        assert_eq!(Value::Int(0).as_bool(), Some(false));
        assert_eq!(Value::Int(1).as_bool(), Some(true));
        assert_eq!(Value::Real(2.0).as_i64(), None);
    }

    #[test]
    fn decodes_runner_kinds() {
        let i32v = RValue {
            payload: u64::from((-5_i32) as u32),
            flags: 0,
            kind: kind::INT32,
        };
        assert_eq!(Value::from_plain_rvalue(&i32v), Some(Value::Int(-5)));
        let game_true = RValue {
            payload: 1.0f64.to_bits(),
            flags: 0,
            kind: kind::BOOL,
        };
        assert_eq!(
            Value::from_plain_rvalue(&game_true),
            Some(Value::Bool(true))
        );
        let unset = RValue {
            payload: 0,
            flags: 0,
            kind: kind::UNSET,
        };
        assert_eq!(Value::from_plain_rvalue(&unset), Some(Value::Undefined));
        let arr = RValue {
            payload: 0xdead,
            flags: 0,
            kind: kind::ARRAY,
        };
        assert_eq!(
            Value::from_plain_rvalue(&arr),
            Some(Value::Other { kind: kind::ARRAY })
        );
        let s = RValue {
            payload: 0,
            flags: 0,
            kind: kind::STRING,
        };
        assert_eq!(Value::from_plain_rvalue(&s), None);
    }

    #[test]
    fn views_and_display() {
        assert_eq!(Value::Bool(true).as_f64(), Some(1.0));
        assert_eq!(Value::Real(0.6).as_bool(), Some(true));
        assert_eq!(Value::from("hi").as_str(), Some("hi"));
        assert_eq!(Value::from(3).to_string(), "3");
        assert_eq!(Value::from("a").to_string(), "\"a\"");
        assert!(Value::Undefined.is_undefined());
    }
}
