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

//! Inline detours on x86-64, the safe part: decoding a function's first
//! instructions, deciding whether they can be moved, and building the bytes of
//! the trampoline, the relay and the patch. `hooks` reads the target's bytes
//! and writes what this module builds (`win` does the memory calls).
//!
//! A detour overwrites the first [`PATCH_LEN`] bytes of a function with
//! `jmp rel32` to a relay (`jmp [rip+0]` and the replacement's address) placed
//! within reach of the function. The instructions that start in those bytes
//! are copied to a trampoline beside the relay, followed by a jump back to the
//! first instruction not copied: calling the trampoline runs the original.
//!
//! A copied instruction keeps its meaning only if it does not depend on where
//! it is. Each form that does is relocated or refused here ([`plan`],
//! [`emit`]):
//!
//! Moved:
//! - a `[rip+disp32]` operand (with or without an immediate after it):
//!   `disp32` rewritten to reach the same address;
//! - `jcc` and `jmp` (`rel8`, `rel32`, `[rip+disp32]`): to the same target,
//!   long or absolute form; a `jmp` ends the copy;
//! - `ret`, `ret imm16`: copied, ends the copy;
//! - a function that ends before [`PATCH_LEN`] bytes: only if the rest of the
//!   patch falls on `int3`/`nop` padding;
//! - a call: only where the caller allows it ([`Calls::Relocate`], the EXE
//!   entry point), and not a call to the copied bytes or the instruction
//!   right after them.
//!
//! Refused:
//! - calls (by default), `loop`, `loope`, `loopne`, `jrcxz`;
//! - a `[rip+disp32]` address or a branch target inside the patched bytes;
//! - `xbegin`, far jumps, calls and returns, `int`, `int3`, `hlt`, `ud*`,
//!   `in`/`out`, `syscall`, moves to and from control and debug registers,
//!   EVEX, XOP, 3DNow!, opcodes invalid in 64-bit mode, an address-size
//!   prefix, a prefix after REX.
//!
//! Not detected: a branch later in the function back into its first
//! [`PATCH_LEN`] bytes (only the copied bytes are decoded; see Limitations in
//! `docs/how-it-works.md`).
//!
//! Anything the decoder does not know is refused, never guessed: the hook is
//! not installed, and the function runs as without it.

#![forbid(unsafe_code)]

use core::fmt;

/// Bytes overwritten at the target: `jmp rel32`.
pub const PATCH_LEN: usize = 5;
/// The longest x86 instruction.
const MAX_INSN: usize = 15;
/// Bytes to read at a target: enough for an instruction starting at the last
/// patched byte.
pub const READ_LEN: usize = PATCH_LEN - 1 + MAX_INSN;
/// A relay: `jmp [rip+0]` followed by the 8-byte destination.
pub const RELAY_LEN: usize = 14;
/// Room for a trampoline: at most [`PATCH_LEN`] instructions of at most 16
/// bytes each once relocated, and the jump back.
pub const TRAMPOLINE_MAX: usize = PATCH_LEN * 16 + RELAY_LEN;
/// How far a relay and its trampoline may be from their target, so that every
/// `rel32` between them, the target's image and what it addresses fits.
pub const NEAR: u64 = 1 << 30;

/// Whether a call in the copied bytes may be relocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Calls {
    /// Refuse the detour (the default for every hooked function).
    Refuse,
    /// Relocate it: for a target where nothing can unwind through the call
    /// (the EXE entry point's call to the CRT's security-cookie set-up).
    Relocate,
}

/// What an instruction's position means for copying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Independent of its address: copied as is.
    Plain,
    /// A `[rip+disp32]` memory operand; the `disp32` is at byte `disp_at` of
    /// the instruction and `target` is the address it reaches.
    RipRelative { disp_at: usize, target: u64 },
    /// `jcc rel8` or `jcc rel32` with condition `cc` (the low opcode nibble).
    Jcc { cc: u8, target: u64 },
    /// `jmp rel8` or `jmp rel32`.
    Jmp { target: u64 },
    /// `call rel32`.
    Call { target: u64 },
}

/// Where execution goes after an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// To the next instruction (a conditional branch may also go elsewhere).
    Next,
    /// Never to the next instruction (`ret`, `jmp`).
    End,
    /// Into a call, then back to the next instruction.
    Call,
}

/// One decoded instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    /// Offset from the start of the function.
    pub offset: usize,
    pub len: usize,
    pub form: Form,
    pub flow: Flow,
}

/// Why a function's first bytes cannot be moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Fewer bytes readable than the instruction needs.
    Truncated,
    /// An opcode the decoder does not know or does not move.
    Unknown,
    /// A prefix combination the decoder does not accept.
    Prefix,
    /// `loop`, `loope`, `loopne` or `jrcxz`.
    Loop,
    /// A call in the copied bytes.
    Call,
    /// A branch whose target is inside the copied bytes.
    BranchInside,
    /// A call (allowed by [`Calls::Relocate`]) to the copied bytes or to the
    /// instruction right after them (`call $+5; pop`: the callee would read
    /// the trampoline's address as its own).
    CallsTheCopy,
    /// A `[rip+disp32]` operand on the bytes the patch overwrites.
    ReadsPatched,
    /// The function ends before the patch's last byte, and what follows is not
    /// padding.
    TooShort,
    /// A displacement that does not reach from the trampoline.
    OutOfRange,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "the instruction runs past the readable bytes",
            Self::Unknown => "an instruction the detour does not move",
            Self::Prefix => "a prefix the detour does not accept",
            Self::Loop => "loop or jrcxz (no long form to relocate to)",
            Self::Call => {
                "a call (its return would land in the trampoline, which has no unwind data)"
            }
            Self::BranchInside => "a branch into the copied bytes",
            Self::CallsTheCopy => "a call to the copied bytes or the instruction after them",
            Self::ReadsPatched => "a rip-relative operand on the patched bytes",
            Self::TooShort => "the function ends within the patch and no padding follows",
            Self::OutOfRange => "a displacement out of reach of the trampoline",
        })
    }
}

/// A refused detour: the instruction at `offset` and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub offset: usize,
    pub why: Why,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at +{:#x}: {}", self.offset, self.why)
    }
}

/// The instructions a detour copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub insns: Vec<Insn>,
    /// Bytes copied (at least [`PATCH_LEN`] unless the last instruction ends
    /// the function and padding fills the rest of the patch).
    pub copied: usize,
}

impl Plan {
    /// Whether the copy ends in `ret`/`jmp` (no jump back is added).
    #[must_use]
    pub fn ends(&self) -> bool {
        self.insns.last().is_some_and(|i| i.flow == Flow::End)
    }

    /// The relocated forms, for the log (`rip+0x0 jcc+0x7`), or `none`.
    #[must_use]
    pub fn relocated(&self) -> String {
        let parts: Vec<String> = self
            .insns
            .iter()
            .filter_map(|i| {
                let what = match i.form {
                    Form::Plain => return None,
                    Form::RipRelative { .. } => "rip",
                    Form::Jcc { .. } => "jcc",
                    Form::Jmp { .. } => "jmp",
                    Form::Call { .. } => "call",
                };
                Some(format!("{what}+{:#x}", i.offset))
            })
            .collect();
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

fn byte(code: &[u8], i: usize) -> Result<u8, Why> {
    code.get(i).copied().ok_or(Why::Truncated)
}

fn rel_target(next: u64, rel: i64) -> u64 {
    next.wrapping_add_signed(rel)
}

/// Length of a `ModRM` operand (`ModRM`, SIB, displacement) starting at `at`, and
/// the offset of its `disp32` if it is `[rip+disp32]`.
fn modrm_len(code: &[u8], at: usize) -> Result<(usize, Option<usize>), Why> {
    let m = byte(code, at)?;
    let (md, rm) = (m >> 6, m & 7);
    if md == 3 {
        return Ok((1, None));
    }
    if md == 0 && rm == 5 {
        return Ok((5, Some(at + 1)));
    }
    let mut n = 1;
    if rm == 4 {
        let sib = byte(code, at + 1)?;
        n += 1;
        if md == 0 && sib & 7 == 5 {
            n += 4;
        }
    }
    n += match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    Ok((n, None))
}

/// A relative branch's kind.
#[derive(Clone, Copy)]
enum Rel {
    Jcc(u8),
    Jmp,
    Call,
}

/// What follows an opcode.
#[derive(Clone, Copy)]
enum Op {
    /// A `ModRM` operand or not, then `imm` immediate bytes.
    Fixed { modrm: bool, imm: usize, flow: Flow },
    /// A relative branch with a `size`-byte displacement.
    Branch { size: usize, rel: Rel },
}

const fn fixed(modrm: bool, imm: usize) -> Op {
    Op::Fixed {
        modrm,
        imm,
        flow: Flow::Next,
    }
}

const fn ends(imm: usize) -> Op {
    Op::Fixed {
        modrm: false,
        imm,
        flow: Flow::End,
    }
}

/// Prefixes seen before the opcode.
#[derive(Clone, Copy, Default)]
struct Prefixes {
    opsize16: bool,
    rex_w: bool,
    /// Any of `66`, `F0`, `F2`, `F3` or a REX: VEX may not follow them.
    vex_blocking: bool,
}

impl Prefixes {
    /// An "Iz" immediate: 16 bits with `66` (and no REX.W), else 32.
    fn iz(self) -> usize {
        if self.opsize16 && !self.rex_w { 2 } else { 4 }
    }
}

fn is_segment(b: u8) -> bool {
    matches!(b, 0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65)
}

/// Legacy prefixes (at most four), then at most one REX right before the
/// opcode. Returns them and the opcode's offset.
fn prefixes(code: &[u8]) -> Result<(Prefixes, usize), Why> {
    let mut p = Prefixes::default();
    let mut i = 0;
    loop {
        let b = byte(code, i)?;
        match b {
            0x66 => p.opsize16 = true,
            0xF0 | 0xF2 | 0xF3 => {}
            // Address size: rip-relative becomes eip-relative and moffs
            // change size; not modelled.
            0x67 => return Err(Why::Prefix),
            _ if is_segment(b) => {}
            _ => break,
        }
        p.vex_blocking |= !is_segment(b);
        i += 1;
        if i > 4 {
            return Err(Why::Prefix);
        }
    }
    if (0x40..=0x4F).contains(&byte(code, i)?) {
        p.rex_w = code[i] & 8 != 0;
        p.vex_blocking = true;
        i += 1;
        // A REX not right before the opcode is ignored by the CPU: refused
        // rather than modelled.
        let b = byte(code, i)?;
        if matches!(b, 0x40..=0x4F | 0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3) || is_segment(b) {
            return Err(Why::Prefix);
        }
    }
    Ok((p, i))
}

/// The one-byte map. `next` is the byte after the opcode (the `ModRM` where
/// there is one: some opcodes are groups chosen by its `reg` field).
fn one_byte(op: u8, p: Prefixes, next: Option<u8>) -> Result<Op, Why> {
    let iz = p.iz();
    let reg = || next.map(|m| (m >> 3) & 7).ok_or(Why::Truncated);
    match op {
        // ALU ops: r/m forms, then AL/eAX with an immediate. The other
        // opcodes below 0x40 (06, 07, 0E, 16, 17, 1E, 1F, 27, 2F, 37, 3F)
        // are invalid in 64-bit mode; 0F, 26, 2E, 36, 3E never get here.
        x if x < 0x40 && x & 7 < 4 => Ok(fixed(true, 0)),
        x if x < 0x40 && x & 7 == 4 => Ok(fixed(false, 1)),
        x if x < 0x40 && x & 7 == 5 => Ok(fixed(false, iz)),
        0x50..=0x5F
        | 0x90..=0x99
        | 0x9B..=0x9F
        | 0xA4..=0xA7
        | 0xAA..=0xAF
        | 0xC9
        | 0xD7
        | 0xF5
        | 0xF8..=0xFD => Ok(fixed(false, 0)),
        0x63 | 0x84..=0x8E | 0xD0..=0xD3 | 0xD8..=0xDF => Ok(fixed(true, 0)),
        0x68 | 0xA9 => Ok(fixed(false, iz)),
        0x69 | 0x81 => Ok(fixed(true, iz)),
        0x6A | 0xA8 | 0xB0..=0xB7 => Ok(fixed(false, 1)),
        0x6B | 0x80 | 0x83 | 0xC0 | 0xC1 => Ok(fixed(true, 1)),
        // pop r/m (other `reg` values: XOP).
        0x8F => match reg()? {
            0 => Ok(fixed(true, 0)),
            _ => Err(Why::Unknown),
        },
        // mov al/eax <-> [moffs64]: an absolute address, movable.
        0xA0..=0xA3 => Ok(fixed(false, 8)),
        0xB8..=0xBF => Ok(fixed(false, if p.rex_w { 8 } else { iz })),
        0xC2 => Ok(ends(2)),
        0xC3 => Ok(ends(0)),
        // mov r/m8, imm8; xabort imm8 (C6 F8).
        0xC6 => match (reg()?, next) {
            (0, _) | (_, Some(0xF8)) => Ok(fixed(true, 1)),
            _ => Err(Why::Unknown),
        },
        // mov r/m, imm (xbegin, C7 F8, is a relative branch: refused).
        0xC7 => match reg()? {
            0 => Ok(fixed(true, iz)),
            _ => Err(Why::Unknown),
        },
        0xC8 => Ok(fixed(false, 3)),
        0x70..=0x7F => Ok(Op::Branch {
            size: 1,
            rel: Rel::Jcc(op & 0xF),
        }),
        0xE0..=0xE3 => Err(Why::Loop),
        0xE8 | 0xE9 | 0xEB if p.opsize16 => Err(Why::Prefix),
        0xE8 => Ok(Op::Branch {
            size: 4,
            rel: Rel::Call,
        }),
        0xE9 => Ok(Op::Branch {
            size: 4,
            rel: Rel::Jmp,
        }),
        0xEB => Ok(Op::Branch {
            size: 1,
            rel: Rel::Jmp,
        }),
        // test r/m, imm (reg 0, 1); not, neg, mul, imul, div, idiv.
        0xF6 => Ok(fixed(true, usize::from(reg()? <= 1))),
        0xF7 => Ok(fixed(true, if reg()? <= 1 { iz } else { 0 })),
        0xFE => match reg()? {
            0 | 1 => Ok(fixed(true, 0)),
            _ => Err(Why::Unknown),
        },
        // inc, dec, call, (far call), jmp, (far jmp), push.
        0xFF => match reg()? {
            0 | 1 | 6 => Ok(fixed(true, 0)),
            2 => Ok(Op::Fixed {
                modrm: true,
                imm: 0,
                flow: Flow::Call,
            }),
            4 => Ok(Op::Fixed {
                modrm: true,
                imm: 0,
                flow: Flow::End,
            }),
            _ => Err(Why::Unknown),
        },
        _ => Err(Why::Unknown),
    }
}

/// The `0F xx` map.
fn two_byte(op: u8, p: Prefixes) -> Result<(usize, Op), Why> {
    let op = match op {
        // 0F 38 xx: ModRM; 0F 3A xx: ModRM and imm8 (the extra opcode byte
        // is counted by the caller through the returned skip).
        0x38 => return Ok((1, fixed(true, 0))),
        0x3A => return Ok((1, fixed(true, 1))),
        0x80..=0x8F if p.opsize16 => return Err(Why::Prefix),
        0x80..=0x8F => Ok(Op::Branch {
            size: 4,
            rel: Rel::Jcc(op & 0xF),
        }),
        0x00..=0x03
        | 0x0D
        | 0x10..=0x1F
        | 0x28..=0x2F
        | 0x40..=0x6F
        | 0x74..=0x76
        | 0x7C..=0x7F
        | 0x90..=0x9F
        | 0xA3
        | 0xA5
        | 0xAB
        | 0xAD..=0xB8
        | 0xBB..=0xC1
        | 0xC3
        | 0xC7
        | 0xD0..=0xFE => Ok(fixed(true, 0)),
        0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4..=0xC6 => Ok(fixed(true, 1)),
        // rdtsc, emms, push/pop fs, cpuid, push/pop gs, bswap.
        0x31 | 0x77 | 0xA0..=0xA2 | 0xA8 | 0xA9 | 0xC8..=0xCF => Ok(fixed(false, 0)),
        // mov to/from CR/DR (0F 20..23: the CPU ignores their `mod` field,
        // so the ModRM layout would guess the length wrong; kernel only),
        // ud0/1/2, syscall/sysret, 3DNow!, vmread/vmwrite, extrq/insertq, ...
        _ => Err(Why::Unknown),
    }?;
    Ok((0, op))
}

/// A VEX instruction (`C4`/`C5` at `i - 1`): bytes of the VEX payload and
/// opcode, and what follows.
fn vex(code: &[u8], i: usize, c4: bool, p: Prefixes) -> Result<(usize, Op), Why> {
    if p.vex_blocking {
        return Err(Why::Prefix);
    }
    let (map, n) = if c4 {
        byte(code, i + 1)?;
        (byte(code, i)? & 0x1F, 2)
    } else {
        byte(code, i)?;
        (1, 1)
    };
    let op = byte(code, i + n)?;
    let what = match map {
        // vzeroupper / vzeroall: no ModRM.
        1 if op == 0x77 => Ok(fixed(false, 0)),
        1 => Ok(fixed(
            true,
            usize::from(matches!(op, 0x70..=0x73 | 0xC2 | 0xC4..=0xC6)),
        )),
        2 => Ok(fixed(true, 0)),
        3 => Ok(fixed(true, 1)),
        _ => Err(Why::Unknown),
    }?;
    Ok((n + 1, what))
}

/// Decode one instruction at the start of `code`, which sits at address `ip`.
///
/// # Errors
/// [`Why`] for an instruction this module does not move (see the module docs).
pub fn decode(code: &[u8], ip: u64) -> Result<(usize, Form, Flow), Why> {
    let (p, mut i) = prefixes(code)?;
    let op = byte(code, i)?;
    i += 1;
    let what = match op {
        0x0F => {
            let (skip, what) = two_byte(byte(code, i)?, p)?;
            i += 1 + skip;
            byte(code, i - 1)?;
            what
        }
        0xC4 | 0xC5 => {
            let (n, what) = vex(code, i, op == 0xC4, p)?;
            i += n;
            what
        }
        _ => one_byte(op, p, code.get(i).copied())?,
    };
    let (len, form, flow) = match what {
        Op::Fixed { modrm, imm, flow } => {
            let (n, disp_at) = if modrm {
                modrm_len(code, i)?
            } else {
                (0, None)
            };
            let len = i + n + imm;
            if len > MAX_INSN {
                return Err(Why::Unknown);
            }
            let form = match disp_at {
                Some(at) => {
                    let disp = i32::from_le_bytes(
                        code.get(at..at + 4)
                            .ok_or(Why::Truncated)?
                            .try_into()
                            .map_err(|_| Why::Truncated)?,
                    );
                    Form::RipRelative {
                        disp_at: at,
                        target: rel_target(ip + len as u64, i64::from(disp)),
                    }
                }
                None => Form::Plain,
            };
            (len, form, flow)
        }
        Op::Branch { size, rel } => {
            let len = i + size;
            let raw = code.get(i..len).ok_or(Why::Truncated)?;
            let r = match raw.len() {
                1 => i64::from(raw[0] as i8),
                _ => i64::from(i32::from_le_bytes(
                    raw.try_into().map_err(|_| Why::Truncated)?,
                )),
            };
            let target = rel_target(ip + len as u64, r);
            match rel {
                Rel::Jcc(cc) => (len, Form::Jcc { cc, target }, Flow::Next),
                Rel::Jmp => (len, Form::Jmp { target }, Flow::End),
                Rel::Call => (len, Form::Call { target }, Flow::Call),
            }
        }
    };
    if code.len() < len {
        return Err(Why::Truncated);
    }
    Ok((len, form, flow))
}

/// Decide what a detour at `ip` copies, given the function's first bytes
/// (`code`, at least [`READ_LEN`] when that many are readable).
///
/// # Errors
/// A [`Refusal`] when any copied instruction cannot be moved (module docs).
pub fn plan(code: &[u8], ip: u64, calls: Calls) -> Result<Plan, Refusal> {
    let mut insns = Vec::new();
    let mut off = 0;
    while off < PATCH_LEN {
        let refuse = |why| Refusal { offset: off, why };
        let (len, form, flow) =
            decode(&code[off.min(code.len())..], ip + off as u64).map_err(refuse)?;
        if flow == Flow::Call && calls == Calls::Refuse {
            return Err(refuse(Why::Call));
        }
        insns.push(Insn {
            offset: off,
            len,
            form,
            flow,
        });
        off += len;
        if flow == Flow::End {
            break;
        }
    }
    let copied = off;
    if copied < PATCH_LEN {
        let pad = code.get(copied..PATCH_LEN).ok_or(Refusal {
            offset: copied,
            why: Why::Truncated,
        })?;
        if !pad.iter().all(|&b| b == 0xCC || b == 0x90) {
            return Err(Refusal {
                offset: copied,
                why: Why::TooShort,
            });
        }
    }
    let covered = ip..ip + copied.max(PATCH_LEN) as u64;
    let patched = ip..ip + PATCH_LEN as u64;
    for i in &insns {
        let refuse = |why| Refusal {
            offset: i.offset,
            why,
        };
        match i.form {
            Form::Call { target } if covered.contains(&target) || target == ip + copied as u64 => {
                return Err(refuse(Why::CallsTheCopy));
            }
            Form::Jcc { target, .. } | Form::Jmp { target } if covered.contains(&target) => {
                return Err(refuse(Why::BranchInside));
            }
            Form::RipRelative { target, .. } if patched.contains(&target) => {
                return Err(refuse(Why::ReadsPatched));
            }
            _ => {}
        }
    }
    Ok(Plan { insns, copied })
}

/// `rel32` from the end of an instruction at `next` to `to`, if it fits.
fn rel32(next: u64, to: u64) -> Option<i32> {
    i32::try_from(to.wrapping_sub(next) as i64).ok()
}

/// `jmp [rip+0]` with the destination after it.
#[must_use]
pub fn relay(to: u64) -> [u8; RELAY_LEN] {
    let mut b = [0u8; RELAY_LEN];
    b[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    b[6..].copy_from_slice(&to.to_le_bytes());
    b
}

/// `jmp` from `at` to `to`: `rel32` when it reaches, else absolute.
fn jump(out: &mut Vec<u8>, at: u64, to: u64) {
    if let Some(r) = rel32(at + 5, to) {
        out.push(0xE9);
        out.extend_from_slice(&r.to_le_bytes());
    } else {
        out.extend_from_slice(&relay(to));
    }
}

/// The trampoline for `plan` at address `at`: the copied instructions of
/// `code` (the target's bytes at `ip`), relocated, and the jump back.
///
/// # Errors
/// [`Why::OutOfRange`] when a relocated `[rip+disp32]` does not reach.
pub fn emit(plan: &Plan, code: &[u8], ip: u64, at: u64) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::with_capacity(TRAMPOLINE_MAX);
    for i in &plan.insns {
        let here = at + out.len() as u64;
        let bytes = &code[i.offset..i.offset + i.len];
        match i.form {
            Form::Plain => out.extend_from_slice(bytes),
            Form::RipRelative { disp_at, target } => {
                let disp = rel32(here + i.len as u64, target).ok_or(Refusal {
                    offset: i.offset,
                    why: Why::OutOfRange,
                })?;
                let start = out.len();
                out.extend_from_slice(bytes);
                out[start + disp_at..start + disp_at + 4].copy_from_slice(&disp.to_le_bytes());
            }
            Form::Jcc { cc, target } => {
                if let Some(r) = rel32(here + 6, target) {
                    out.extend_from_slice(&[0x0F, 0x80 | cc]);
                    out.extend_from_slice(&r.to_le_bytes());
                } else {
                    // The inverted condition skips the absolute jump.
                    out.extend_from_slice(&[0x70 | (cc ^ 1), RELAY_LEN as u8]);
                    out.extend_from_slice(&relay(target));
                }
            }
            Form::Jmp { target } => jump(&mut out, here, target),
            Form::Call { target } => {
                if let Some(r) = rel32(here + 5, target) {
                    out.push(0xE8);
                    out.extend_from_slice(&r.to_le_bytes());
                } else {
                    // call [rip+2]; jmp +8; the destination.
                    out.extend_from_slice(&[0xFF, 0x15, 2, 0, 0, 0, 0xEB, 8]);
                    out.extend_from_slice(&target.to_le_bytes());
                }
            }
        }
    }
    if !plan.ends() {
        let here = at + out.len() as u64;
        jump(&mut out, here, ip + plan.copied as u64);
    }
    Ok(out)
}

/// The patch at `ip`: `jmp rel32` to the relay at `relay_at`, or `None` if
/// it does not reach.
#[must_use]
pub fn patch(ip: u64, relay_at: u64) -> Option<[u8; PATCH_LEN]> {
    let r = rel32(ip + PATCH_LEN as u64, relay_at)?;
    let mut b = [0xE9, 0, 0, 0, 0];
    b[1..].copy_from_slice(&r.to_le_bytes());
    Some(b)
}

/// Hex of `bytes` for the log (`48 83 ec 28`).
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic function starts, one per instruction form a compiled function
    // begins with (builtin routines, Win32 export stubs, the CRT entry point);
    // the bytes here are made up.

    /// `sub rsp, 28h; cmp qword [rip+X], 0; je ...`: an immediate after the
    /// displacement, which the relocated copy must keep (the new displacement
    /// goes at its decoded place, not over the instruction's last four bytes).
    const RIP_CMP_QWORD_IMM8: [u8; 16] = [
        0x48, 0x83, 0xec, 0x28, 0x48, 0x83, 0x3d, 0x10, 0x20, 0x30, 0x00, 0x00, 0x74, 0x10, 0x48,
        0x8d,
    ];
    /// `sub rsp, 28h; cmp byte [rip+X], 0; je ...`.
    const RIP_CMP_BYTE_IMM8: [u8; 16] = [
        0x48, 0x83, 0xec, 0x28, 0x80, 0x3d, 0x44, 0x33, 0x22, 0x00, 0x00, 0x74, 0x18, 0x8b, 0x05,
        0x10,
    ];
    /// `mov dword [rip+X], -1; ret` (an imm32 after the displacement).
    const RIP_MOV_DWORD_IMM32: [u8; 16] = [
        0xc7, 0x05, 0x11, 0x22, 0x33, 0x00, 0xff, 0xff, 0xff, 0xff, 0xc3, 0xcc, 0xcc, 0xcc, 0xcc,
        0xcc,
    ];
    /// `sub dword [rip+X], 1; jns ...`.
    const RIP_SUB_DWORD_IMM8: [u8; 16] = [
        0x83, 0x2d, 0x55, 0x44, 0x33, 0x00, 0x01, 0x79, 0x74, 0xc7, 0x05, 0x10, 0x20, 0x30, 0x00,
        0x00,
    ];
    /// `sub rsp, 28h; call +100h; add rsp, 28h; ret` (the CRT entry point's
    /// shape, and that of builtins that call first).
    const SUB_RSP_CALL: [u8; 16] = [
        0x48, 0x83, 0xec, 0x28, 0xe8, 0x00, 0x01, 0x00, 0x00, 0x48, 0x83, 0xc4, 0x28, 0xc3, 0xcc,
        0xcc,
    ];
    /// `test r8d, r8d; jle +13h; ...`.
    const TEST_JLE8: [u8; 16] = [
        0x45, 0x85, 0xc0, 0x7e, 0x13, 0x48, 0x8b, 0x4c, 0x24, 0x30, 0x83, 0x79, 0x08, 0x05, 0x75,
        0x08,
    ];
    /// `jmp rel32` (a tail call, -1000h), padding.
    const JMP_REL32: [u8; 8] = [0xe9, 0x00, 0xf0, 0xff, 0xff, 0xcc, 0xcc, 0xcc];
    /// `xor ecx, ecx; jmp rel32` (-2000h).
    const XOR_JMP_REL32: [u8; 8] = [0x33, 0xc9, 0xe9, 0x00, 0xe0, 0xff, 0xff, 0xcc];
    /// `ret 8` and padding.
    const RET_IMM16: [u8; 8] = [0xc2, 0x08, 0x00, 0x90, 0xcc, 0xcc, 0xcc, 0xcc];
    /// `push rbx; sub rsp, 20h; mov eax, [rsp+50h]; ...`.
    const PUSH_SUB: [u8; 14] = [
        0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x8b, 0x44, 0x24, 0x50, 0x89, 0x44, 0x24, 0x28,
    ];
    /// `mov rax, rsp; mov [rax+18h], rbx; mov [rax+20h], rdi; push rbp`.
    const MOV_RAX_RSP: [u8; 12] = [
        0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x18, 0x48, 0x89, 0x78, 0x20, 0x55,
    ];
    /// `sub rsp, 38h; mov rax, imm64; ...`.
    const MOV_IMM64: [u8; 16] = [
        0x48, 0x83, 0xec, 0x38, 0x48, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x24, 0x40, 0xc7,
        0x41,
    ];
    /// `rex.w jmp [rip+X]` (an import stub).
    const REXW_JMP_RIP: [u8; 8] = [0x48, 0xff, 0x25, 0x00, 0x10, 0x00, 0x00, 0xcc];

    const IP: u64 = 0x1_4012_3450;
    /// A trampoline 16 MB away (as `win::alloc_code_near` finds one).
    const AT: u64 = IP + 0x0100_0000;

    fn disp_at(code: &[u8], at: usize) -> i32 {
        i32::from_le_bytes(code[at..at + 4].try_into().unwrap())
    }

    /// The address a `[rip+disp32]` in `code` at `off` (length `len`, disp at
    /// `d`) reaches when the code sits at `base`.
    fn reach(code: &[u8], base: u64, off: usize, len: usize, d: usize) -> u64 {
        (base + (off + len) as u64).wrapping_add_signed(i64::from(disp_at(code, off + d)))
    }

    /// `t` ends in `jmp rel32` to `to`.
    fn jump_back(t: &[u8], at: u64, to: u64) {
        let n = t.len();
        assert_eq!(t[n - 5], 0xE9);
        assert_eq!(
            (at + n as u64).wrapping_add_signed(i64::from(disp_at(t, n - 4))),
            to
        );
    }

    #[test]
    fn a_rip_compare_with_an_immediate_is_relocated_with_the_immediate_kept() {
        let code = RIP_CMP_QWORD_IMM8;
        let plan = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(plan.copied, 12); // sub rsp (4) + cmp qword [rip+X], 0 (8)
        let target = reach(&code, IP, 4, 8, 3);
        assert_eq!(plan.insns[1].form, Form::RipRelative { disp_at: 3, target });
        let t = emit(&plan, &code, IP, AT).unwrap();
        assert_eq!(t[..7], code[..7]); // sub rsp; 48 83 3d unchanged
        assert_eq!(reach(&t, AT, 4, 8, 3), target);
        assert_eq!(t[11], 0x00, "the immediate is the instruction's last byte");
        jump_back(&t, AT, IP + 12);
        assert_eq!(t.len(), 12 + 5);
    }

    #[test]
    fn rip_operands_with_immediates_after_them() {
        for (code, off, len, d) in [
            (&RIP_CMP_BYTE_IMM8[..], 4, 7, 2),    // cmp byte [rip], imm8
            (&RIP_MOV_DWORD_IMM32[..], 0, 10, 2), // mov dword [rip], imm32
            (&RIP_SUB_DWORD_IMM8[..], 0, 7, 2),   // sub dword [rip], imm8
        ] {
            let plan = plan(code, IP, Calls::Refuse).unwrap();
            let target = reach(code, IP, off, len, d);
            let i = plan.insns.iter().find(|i| i.offset == off).unwrap();
            assert_eq!(i.len, len);
            assert_eq!(i.form, Form::RipRelative { disp_at: d, target });
            let t = emit(&plan, code, IP, AT).unwrap();
            assert_eq!(reach(&t, AT, off, len, d), target);
            assert_eq!(t[off + d + 4..off + len], code[off + d + 4..off + len]);
            jump_back(&t, AT, IP + plan.copied as u64);
        }
    }

    #[test]
    fn rip_operands_without_immediates_and_rip_jumps() {
        // rex.w jmp [rip+X]: relocated, ends the copy (no jump back).
        let code = REXW_JMP_RIP;
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(p.copied, 7);
        assert!(p.ends());
        let t = emit(&p, &code, IP, AT).unwrap();
        assert_eq!(t.len(), 7);
        assert_eq!(reach(&t, AT, 0, 7, 3), reach(&code, IP, 0, 7, 3));
        // lea rax, [rip+X]; then plain code.
        let code = [0x48, 0x8d, 0x05, 0x10, 0x20, 0x30, 0x00, 0xc3];
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        let t = emit(&p, &code, IP, AT).unwrap();
        assert_eq!(reach(&t, AT, 0, 7, 3), reach(&code, IP, 0, 7, 3));
    }

    #[test]
    fn a_rip_operand_out_of_reach_is_refused() {
        let code = RIP_CMP_QWORD_IMM8;
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(
            emit(&p, &code, IP, IP + (3 << 30)),
            Err(Refusal {
                offset: 4,
                why: Why::OutOfRange
            })
        );
    }

    #[test]
    fn a_rip_operand_on_the_patched_bytes_is_refused() {
        // nop; nop; mov al, [rip-8] (reaches ip+0, a patched byte); ret
        let code = [0x90, 0x90, 0x8a, 0x05, 0xf8, 0xff, 0xff, 0xff, 0xc3];
        assert_eq!(
            plan(&code, IP, Calls::Refuse),
            Err(Refusal {
                offset: 2,
                why: Why::ReadsPatched
            })
        );
    }

    #[test]
    fn jcc_rel8_out_of_the_copy_becomes_rel32() {
        let code = TEST_JLE8;
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(p.copied, 5);
        let target = IP + 5 + 0x13;
        assert_eq!(p.insns[1].form, Form::Jcc { cc: 0xE, target });
        let t = emit(&p, &code, IP, AT).unwrap();
        assert_eq!(t[..3], code[..3]);
        assert_eq!(t[3..5], [0x0F, 0x8E]); // jle rel32
        assert_eq!(
            (AT + 9).wrapping_add_signed(i64::from(disp_at(&t, 5))),
            target
        );
        jump_back(&t, AT, IP + 5);
    }

    #[test]
    fn jcc_out_of_rel32_reach_becomes_an_inverted_jcc_over_an_absolute_jump() {
        let code = TEST_JLE8;
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        let t = emit(&p, &code, IP, IP + (5 << 30)).unwrap();
        assert_eq!(t[3..5], [0x7F, 14]); // jg +14 (not jle) over the jump
        assert_eq!(t[5..11], [0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(t[11..19].try_into().unwrap()),
            IP + 5 + 0x13
        );
        // And the jump back is absolute too.
        assert_eq!(t[19..25], [0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(t[25..33].try_into().unwrap()), IP + 5);
    }

    #[test]
    fn jcc_rel32_is_decoded_and_relocated() {
        // test eax, eax; jne rel32 +0x100
        let code = [0x85, 0xc0, 0x0f, 0x85, 0x00, 0x01, 0x00, 0x00, 0xc3];
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(p.copied, 8);
        assert_eq!(
            p.insns[1].form,
            Form::Jcc {
                cc: 5,
                target: IP + 8 + 0x100
            }
        );
        let t = emit(&p, &code, IP, AT).unwrap();
        assert_eq!(t[2..4], [0x0F, 0x85]);
        assert_eq!(
            (AT + 8).wrapping_add_signed(i64::from(disp_at(&t, 4))),
            IP + 0x108
        );
    }

    #[test]
    fn jmp_rel32_and_rel8_end_the_copy() {
        for (code, off, target) in [
            (&JMP_REL32[..], 0, (IP + 5).wrapping_sub(0x1000)),
            (&XOR_JMP_REL32[..], 2, (IP + 7).wrapping_sub(0x2000)),
            (&[0xeb, 0x10, 0xcc, 0xcc, 0xcc, 0xcc][..], 0, IP + 0x12),
        ] {
            let p = plan(code, IP, Calls::Refuse).unwrap();
            assert!(p.ends());
            assert_eq!(p.insns.last().unwrap().form, Form::Jmp { target });
            let t = emit(&p, code, IP, AT).unwrap();
            assert_eq!(t.len(), off + 5, "no jump back after a jmp");
            jump_back(&t, AT, target);
        }
    }

    #[test]
    fn a_call_in_the_copy_is_refused_unless_allowed() {
        assert_eq!(
            plan(&SUB_RSP_CALL, IP, Calls::Refuse),
            Err(Refusal {
                offset: 4,
                why: Why::Call
            })
        );
        // call [rip+X] too.
        let code = [0xff, 0x15, 0x10, 0x00, 0x00, 0x00, 0xc3];
        assert_eq!(plan(&code, IP, Calls::Refuse).unwrap_err().why, Why::Call);
        // The entry point's call, relocated.
        let p = plan(&SUB_RSP_CALL, IP, Calls::Relocate).unwrap();
        assert_eq!(p.copied, 9);
        let target = IP + 9 + 0x100;
        assert_eq!(p.insns[1].form, Form::Call { target });
        let t = emit(&p, &SUB_RSP_CALL, IP, AT).unwrap();
        assert_eq!(t[4], 0xE8);
        assert_eq!(
            (AT + 9).wrapping_add_signed(i64::from(disp_at(&t, 5))),
            target
        );
        jump_back(&t, AT, IP + 9);
        // Out of rel32 reach: call [rip+2]; jmp +8; the address.
        let t = emit(&p, &SUB_RSP_CALL, IP, IP + (5 << 30)).unwrap();
        assert_eq!(t[4..12], [0xFF, 0x15, 2, 0, 0, 0, 0xEB, 8]);
        assert_eq!(u64::from_le_bytes(t[12..20].try_into().unwrap()), target);
    }

    #[test]
    fn a_call_to_the_copy_or_the_next_instruction_is_refused_even_when_allowed() {
        // call $+5; pop rax (a wrapper finding its own address).
        let code = [0xe8, 0, 0, 0, 0, 0x58, 0x48, 0x2d, 5, 0, 0, 0, 0xc3];
        assert_eq!(
            plan(&code, IP, Calls::Relocate),
            Err(Refusal {
                offset: 0,
                why: Why::CallsTheCopy
            })
        );
        // sub rsp, 28h; call $+5 (to the first byte not copied).
        let code = [0x48, 0x83, 0xec, 0x28, 0xe8, 0, 0, 0, 0, 0x58, 0xc3];
        assert_eq!(
            plan(&code, IP, Calls::Relocate).unwrap_err().why,
            Why::CallsTheCopy
        );
        // nop; call -6 (to the function's start, inside the copy).
        let code = [0x90, 0xe8, 0xfa, 0xff, 0xff, 0xff, 0xc3];
        assert_eq!(
            plan(&code, IP, Calls::Relocate).unwrap_err().why,
            Why::CallsTheCopy
        );
    }

    #[test]
    fn loops_are_refused() {
        for op in [0xe0, 0xe1, 0xe2, 0xe3] {
            let code = [op, 0x10, 0x90, 0x90, 0x90, 0xc3];
            assert_eq!(plan(&code, IP, Calls::Refuse).unwrap_err().why, Why::Loop);
        }
        // jecxz is 67 e3: refused at the address-size prefix.
        let code = [0x67, 0xe3, 0x10, 0x90, 0x90, 0xc3];
        assert_eq!(plan(&code, IP, Calls::Refuse).unwrap_err().why, Why::Prefix);
    }

    #[test]
    fn a_branch_into_the_copy_is_refused() {
        // test ecx, ecx; je +0 (to the nop at +4, a copied byte); nop; ... (je +1
        // would reach +5, the first byte not copied: allowed).
        let code = [0x85, 0xc9, 0x74, 0x00, 0x90, 0x90, 0xc3];
        assert_eq!(
            plan(&code, IP, Calls::Refuse),
            Err(Refusal {
                offset: 2,
                why: Why::BranchInside
            })
        );
        // A jmp back to the function's start.
        let code = [0x90, 0x90, 0x90, 0xeb, 0xfb, 0xcc];
        assert_eq!(
            plan(&code, IP, Calls::Refuse).unwrap_err().why,
            Why::BranchInside
        );
    }

    /// The limitation the module documents: a branch from after the copied
    /// bytes back into the patch is not looked for.
    #[test]
    fn a_loop_back_from_after_the_copy_is_not_detected() {
        // xor eax, eax; L: add eax, ecx; dec ecx; jnz L (+2); ret
        let code = [0x31, 0xc0, 0x01, 0xc8, 0xff, 0xc9, 0x75, 0xfa, 0xc3];
        let p = plan(&code, IP, Calls::Refuse).unwrap();
        assert_eq!(p.copied, 6, "the jnz at +6 is not copied, so not seen");
    }

    #[test]
    fn short_functions_need_padding() {
        let p = plan(&RET_IMM16, IP, Calls::Refuse).unwrap();
        assert_eq!(p.copied, 3);
        assert!(p.ends());
        assert_eq!(emit(&p, &RET_IMM16, IP, AT).unwrap(), [0xc2, 8, 0]);
        // xor eax, eax; ret; then the next function.
        let code = [0x31, 0xc0, 0xc3, 0x48, 0x89, 0x5c, 0x24, 0x08];
        assert_eq!(
            plan(&code, IP, Calls::Refuse),
            Err(Refusal {
                offset: 3,
                why: Why::TooShort
            })
        );
    }

    #[test]
    fn ordinary_prologues_are_copied_as_they_are() {
        for (code, copied) in [
            (&PUSH_SUB[..], 6),
            (&MOV_RAX_RSP[..], 7),
            (&MOV_IMM64[..], 14),
        ] {
            let p = plan(code, IP, Calls::Refuse).unwrap();
            assert_eq!(p.copied, copied);
            assert_eq!(p.relocated(), "none");
            let t = emit(&p, code, IP, AT).unwrap();
            assert_eq!(t[..copied], code[..copied]);
            jump_back(&t, AT, IP + copied as u64);
        }
    }

    #[test]
    fn unknown_or_unmoved_instructions_are_refused() {
        let cases: [(&[u8], Why); 19] = [
            (&[0xcc, 0x90, 0x90, 0x90, 0x90], Why::Unknown), // int3
            (&[0x0f, 0x0b, 0x90, 0x90, 0x90], Why::Unknown), // ud2
            (&[0x62, 0xf1, 0x7c, 0x48, 0x10, 0x00], Why::Unknown), // EVEX
            (&[0x8f, 0xe8, 0x78, 0xc2, 0xc0, 0x00], Why::Unknown), // XOP
            (&[0x0f, 0x0f, 0xc0, 0x9e, 0x90], Why::Unknown), // 3DNow!
            (&[0xc7, 0xf8, 0x00, 0x01, 0x00, 0x00], Why::Unknown), // xbegin
            (&[0xff, 0x2d, 0, 0, 0, 0], Why::Unknown),       // jmp far
            (&[0xcb, 0x90, 0x90, 0x90, 0x90], Why::Unknown), // retf
            (&[0x0f, 0x05, 0x90, 0x90, 0x90], Why::Unknown), // syscall
            (&[0x06, 0x90, 0x90, 0x90, 0x90], Why::Unknown), // invalid in 64-bit
            (&[0x0f, 0x20, 0x05, 0, 0, 0, 0], Why::Unknown), // mov rbp, cr0 (3 bytes, not 7)
            (&[0x0f, 0x21, 0xc0, 0x90, 0x90], Why::Unknown), // mov rax, dr0
            (&[0x0f, 0x22, 0xd8, 0x90, 0x90], Why::Unknown), // mov cr3, rax
            (&[0x0f, 0x23, 0xf8, 0x90, 0x90], Why::Unknown), // mov dr7, rax
            (&[0x67, 0x8b, 0x05, 0, 0, 0, 0], Why::Prefix),  // addr32
            (&[0x48, 0x66, 0x90, 0x90, 0x90], Why::Prefix),  // prefix after REX
            (&[0x66, 0xc5, 0xf8, 0x77, 0x90], Why::Prefix),  // 66 before VEX
            (&[0x66, 0xe8, 0, 0, 0x90], Why::Prefix),        // call rel16
            (&[0x48, 0x83, 0x3d, 0x10, 0x20], Why::Truncated),
        ];
        for (code, why) in cases {
            assert_eq!(
                plan(code, IP, Calls::Relocate).unwrap_err().why,
                why,
                "{code:02x?}"
            );
        }
    }

    #[test]
    fn instruction_lengths() {
        let cases: [(&[u8], usize); 30] = [
            (&[0x48, 0x89, 0x5c, 0x24, 0x08], 5),        // mov [rsp+8], rbx
            (&[0x48, 0x81, 0xec, 0x88, 0, 0, 0], 7),     // sub rsp, imm32
            (&[0x4c, 0x8b, 0xdc], 3),                    // mov r11, rsp
            (&[0x40, 0x53], 2),                          // push rbx
            (&[0x66, 0x0f, 0x1f, 0x44, 0, 0], 6),        // nop word
            (&[0x0f, 0x1f, 0x80, 0, 0, 0, 0], 7),        // nop dword [rax+0]
            (&[0x48, 0xb8, 1, 2, 3, 4, 5, 6, 7, 8], 10), // mov rax, imm64
            (&[0x66, 0xb8, 1, 2], 4),                    // mov ax, imm16
            (&[0xf3, 0x0f, 0x1e, 0xfa], 4),              // endbr64
            (&[0xc5, 0xf8, 0x77], 3),                    // vzeroupper
            (&[0xc5, 0xf8, 0x29, 0x74, 0x24, 0x20], 6),  // vmovaps [rsp+20h], xmm6
            (&[0xc4, 0xe3, 0x79, 0x0f, 0xc1, 0x08], 6),  // vpalignr (map 3, imm8)
            (&[0x0f, 0x29, 0x74, 0x24, 0x40], 5),        // movaps [rsp+40h], xmm6
            (&[0x66, 0x0f, 0x3a, 0x0f, 0xc1, 0x08], 6),  // palignr
            (&[0x66, 0x0f, 0x38, 0x00, 0xc1], 5),        // pshufb
            (&[0xf6, 0x05, 0, 0, 0, 0, 1], 7),           // test byte [rip], imm8
            (&[0xf7, 0x05, 0, 0, 0, 0, 1, 0, 0, 0], 10), // test dword [rip], imm32
            (&[0xf7, 0xd8], 2),                          // neg eax
            (&[0x66, 0x83, 0x3d, 0, 0, 0, 0, 0], 8),     // cmp word [rip], imm8
            (&[0x66, 0xc7, 0x05, 0, 0, 0, 0, 1, 0], 9),  // mov word [rip], imm16
            (&[0x48, 0xc7, 0x05, 0, 0, 0, 0, 1, 0, 0, 0], 11), // mov qword [rip], imm32
            (&[0x0f, 0xba, 0x25, 0, 0, 0, 0, 5], 8),     // bt dword [rip], 5
            (&[0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0], 8),  // mov rax, [abs32]
            (&[0x8b, 0x44, 0x8d, 0x10], 4),              // mov eax, [rbp+rcx*4+10h]
            (&[0x8b, 0x84, 0x24, 0, 1, 0, 0], 7),        // mov eax, [rsp+100h]
            (&[0x65, 0x48, 0x8b, 0x04, 0x25, 0x30, 0, 0, 0], 9), // mov rax, gs:[30h]
            (&[0xa1, 1, 2, 3, 4, 5, 6, 7, 8], 9),        // mov eax, [moffs64]
            (&[0xc8, 0x10, 0, 0], 4),                    // enter
            (&[0x0f, 0x31], 2),                          // rdtsc
            (&[0xf0, 0x0f, 0xb1, 0x0d, 0, 0, 0, 0], 8),  // lock cmpxchg [rip], ecx
        ];
        for (code, len) in cases {
            let (n, _, _) = decode(code, IP).unwrap_or_else(|w| panic!("{code:02x?}: {w}"));
            assert_eq!(n, len, "{code:02x?}");
        }
        // The rip forms report where their displacement is.
        let (_, form, _) = decode(&[0x66, 0xc7, 0x05, 0, 0, 0, 0, 1, 0], IP).unwrap();
        assert!(matches!(form, Form::RipRelative { disp_at: 3, .. }));
        let (_, form, _) = decode(&[0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0], IP).unwrap();
        assert_eq!(
            form,
            Form::Plain,
            "an absolute [disp32] is not rip-relative"
        );
    }

    #[test]
    fn relay_and_patch() {
        let r = relay(0x7FF6_1234_5678);
        assert_eq!(r[..6], [0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(
            u64::from_le_bytes(r[6..].try_into().unwrap()),
            0x7FF6_1234_5678
        );
        let p = patch(IP, AT).unwrap();
        assert_eq!(p[0], 0xE9);
        assert_eq!((IP + 5).wrapping_add_signed(i64::from(disp_at(&p, 1))), AT);
        assert_eq!(patch(IP, IP + (3 << 30)), None);
    }

    #[test]
    fn refusals_read_as_one_line() {
        let r = plan(&SUB_RSP_CALL, IP, Calls::Refuse).unwrap_err();
        assert_eq!(
            r.to_string(),
            "at +0x4: a call (its return would land in the trampoline, which has no unwind data)"
        );
        let p = plan(&RIP_CMP_QWORD_IMM8, IP, Calls::Refuse).unwrap();
        assert_eq!(p.relocated(), "rip+0x4");
    }
}
