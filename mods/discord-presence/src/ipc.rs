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

//! Minimal Discord RPC client over the local IPC named pipe, in safe Rust.
//!
//! Discord (desktop app) listens on `\\.\pipe\discord-ipc-0` … `-9`. Every
//! message is a frame: `u32 LE opcode`, `u32 LE payload length`, UTF-8 JSON.
//! The client sends a handshake (`op 0`, `{"v":1,"client_id":"…"}`), Discord
//! answers with a `READY` dispatch, then commands go as `op 1` frames and each
//! gets one reply frame. Every reply is read so the pipe never fills up.
//!
//! Std's `File` opens the pipe in synchronous mode, so a read and a write on the
//! same handle serialise. The worker thread therefore always writes, then reads
//! the reply, and never reads speculatively.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};

pub const OP_HANDSHAKE: u32 = 0;
pub const OP_FRAME: u32 = 1;
pub const OP_CLOSE: u32 = 2;
pub const OP_PING: u32 = 3;
pub const OP_PONG: u32 = 4;

/// Pipe indices Discord may use (one per running client: stable, PTB, canary, ...).
pub const PIPE_COUNT: u32 = 10;
/// Refuse absurd frames instead of allocating whatever length the peer claims.
pub const MAX_FRAME: u32 = 64 * 1024;

/// Path of IPC pipe `n`.
#[must_use]
pub fn pipe_path(n: u32) -> String {
    format!(r"\\.\pipe\discord-ipc-{n}")
}

/// Serialise one frame.
#[must_use]
pub fn encode(op: u32, json: &str) -> Vec<u8> {
    let len = u32::try_from(json.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(8 + json.len());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(json.as_bytes());
    out
}

/// Read one frame: `(opcode, payload)`.
pub fn read_frame(r: &mut impl Read) -> io::Result<(u32, String)> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header)?;
    let op = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    Ok((op, String::from_utf8_lossy(&body).into_owned()))
}

/// JSON string literal (with quotes) for `s`.
#[must_use]
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Handshake payload.
#[must_use]
pub fn handshake_json(client_id: &str) -> String {
    format!(r#"{{"v":1,"client_id":{}}}"#, json_str(client_id))
}

/// `SET_ACTIVITY` command. `activity_json` is a JSON object, or `null` to clear.
#[must_use]
pub fn set_activity_json(pid: u32, activity_json: &str, nonce: u64) -> String {
    format!(
        r#"{{"cmd":"SET_ACTIVITY","args":{{"pid":{pid},"activity":{activity_json}}},"nonce":"{nonce}"}}"#
    )
}

/// Whether a reply frame reports an error (`"evt":"ERROR"`).
#[must_use]
pub fn is_error_reply(json: &str) -> bool {
    json.contains(r#""evt":"ERROR""#)
}

/// A connected, handshaken IPC session.
#[derive(Debug)]
pub struct Client {
    pipe: File,
    nonce: u64,
}

impl Client {
    /// Open the first available pipe and handshake. Fails if Discord isn't running
    /// or rejects the client id.
    pub fn connect(client_id: &str) -> io::Result<Self> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, "no Discord IPC pipe");
        for n in 0..PIPE_COUNT {
            match OpenOptions::new().read(true).write(true).open(pipe_path(n)) {
                Ok(pipe) => {
                    let mut client = Self { pipe, nonce: 0 };
                    match client.handshake(client_id) {
                        Ok(()) => return Ok(client),
                        Err(e) => last = e,
                    }
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn handshake(&mut self, client_id: &str) -> io::Result<()> {
        self.pipe
            .write_all(&encode(OP_HANDSHAKE, &handshake_json(client_id)))?;
        let reply = self.read_reply()?;
        if reply.contains(r#""evt":"READY""#) {
            Ok(())
        } else {
            Err(io::Error::other(format!("handshake rejected: {reply}")))
        }
    }

    /// Read frames until a command reply, answering pings on the way.
    fn read_reply(&mut self) -> io::Result<String> {
        loop {
            let (op, body) = read_frame(&mut self.pipe)?;
            match op {
                OP_PING => self.pipe.write_all(&encode(OP_PONG, &body))?,
                OP_CLOSE => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        format!("Discord closed the connection: {body}"),
                    ));
                }
                _ => return Ok(body),
            }
        }
    }

    /// Set (`Some(json object)`) or clear (`None`) the activity for process `pid`.
    pub fn set_activity(&mut self, pid: u32, activity_json: Option<&str>) -> io::Result<()> {
        self.nonce += 1;
        let cmd = set_activity_json(pid, activity_json.unwrap_or("null"), self.nonce);
        self.pipe.write_all(&encode(OP_FRAME, &cmd))?;
        let reply = self.read_reply()?;
        if is_error_reply(&reply) {
            return Err(io::Error::other(format!("SET_ACTIVITY failed: {reply}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let bytes = encode(OP_FRAME, r#"{"a":1}"#);
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &7u32.to_le_bytes());
        let (op, body) = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!((op, body.as_str()), (OP_FRAME, r#"{"a":1}"#));
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
        assert!(read_frame(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let bytes = encode(OP_FRAME, "{}");
        assert!(read_frame(&mut &bytes[..9]).is_err());
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_str(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_str("x\ny\u{1}"), r#""x\ny\u0001""#);
        assert_eq!(json_str("Wave 5 · Level 3"), "\"Wave 5 · Level 3\"");
    }

    #[test]
    fn command_shapes() {
        assert_eq!(handshake_json("123"), r#"{"v":1,"client_id":"123"}"#);
        assert_eq!(
            set_activity_json(42, "null", 7),
            r#"{"cmd":"SET_ACTIVITY","args":{"pid":42,"activity":null},"nonce":"7"}"#
        );
        assert!(is_error_reply(
            r#"{"cmd":"SET_ACTIVITY","evt":"ERROR","data":{}}"#
        ));
        assert!(!is_error_reply(r#"{"cmd":"SET_ACTIVITY","evt":null}"#));
    }
}
