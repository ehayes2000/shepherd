//! The bytes of an escape sequence or UTF-8 character the parser has started but not finished, so a snapshot can
//! carry them and the copy's parser ends up in the same state.
//!
//! The parser keeps its state private and can't be cloned, so the state is rebuilt from what it was fed: a fresh
//! `vte::Parser` fed the same bytes from a point where the real one was in ground ends in the same state. Facts about
//! vte 0.15 this relies on:
//! - ESC moves the parser to the escape state with cleared parameters from every state, so the bytes before the
//!   last ESC never matter, and only ESC leaves ground (apart from a partial UTF-8 char).
//! - Whether a parser is back in ground can be asked by feeding it a probe character: only a parser in ground
//!   prints it straight away. One holding part of a UTF-8 char prints U+FFFD first.
//! - A sequence ends with a callback (a dispatch, `unhook`, or executing CAN / SUB), except a CSI that was ignoring
//!   its parameters, which ends silently on its final byte (0x40..=0x7E).
//!
//! Probing destroys the parser it asks, so a sequence still open after a feed is followed by a second parser fed
//! only the new bytes, which keeps the work linear however long the sequence runs.
//!
//! vte handles invalid UTF-8 differently depending on where the input was split, and the replay reads its bytes in
//! one piece, so for invalid input the copy follows the unsplit result. Valid UTF-8 is unaffected.

use alacritty_terminal::vte::{Params, Parser, Perform};

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// Printed in ground, a final byte in an escape or CSI sequence, and swallowed by strings.
const PROBE: u8 = b'Z';
/// The most bytes of one open sequence that are carried. Past this the copy gets only a prefix. It still swallows
/// the rest of the sequence and ends it on the same byte, but what the sequence does then may differ: an OSC is
/// dispatched with its payload cut short (shepherd ignores titles and the clipboard, and an OSC 4 colour list is
/// applied only as far as the prefix), and a CSI or escape sequence is dispatched with the parameters it had at the
/// cut. alacritty ignores DCS, SOS, PM and APC payloads anyway.
const MAX_CARRIED: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Unfinished {
    /// Starts at an ESC or at the lead byte of a partial char; empty while the parser is in ground.
    bytes: Vec<u8>,
    /// While a sequence is open: a parser that has read all of it so far, to find where it ends.
    open: Option<Parser>,
}

impl Unfinished {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Follows `parsed`, the next bytes the parser consumed.
    pub(crate) fn advance(&mut self, mut parsed: &[u8]) {
        if let Some(esc) = parsed.iter().rposition(|&b| b == ESC) {
            self.bytes.clear();
            self.open = None;
            parsed = &parsed[esc..];
        } else if self.open.is_some() {
            match self.sequence_end(parsed) {
                Some(end) => {
                    self.bytes.clear();
                    self.open = None;
                    parsed = &parsed[end..];
                }
                None => {
                    self.carry(parsed);
                    return;
                }
            }
        }
        self.settle(parsed);
    }

    /// The parser is in ground, or holds the partial char in `bytes`, and reads `parsed`.
    fn settle(&mut self, parsed: &[u8]) {
        if parsed.is_empty() {
            return;
        }
        let joined;
        let region = if self.bytes.is_empty() {
            parsed
        } else {
            self.bytes.extend_from_slice(parsed);
            joined = std::mem::take(&mut self.bytes);
            &joined[..]
        };
        match probe(region) {
            State::Ground => self.bytes = Vec::new(),
            State::InChar => {
                // The incomplete char is the last thing fed, and nothing before it can absorb its lead byte.
                let partial = region
                    .utf8_chunks()
                    .last()
                    .map_or(0, |chunk| chunk.invalid().len());
                self.bytes = region[region.len() - partial..].to_vec();
            }
            State::InSequence => {
                let mut parser = Parser::new();
                parser.advance(&mut Ignore, region);
                self.open = Some(parser);
                self.bytes = Vec::new();
                self.carry(region);
            }
        }
    }

    /// Where in `parsed` (with no ESC) the open sequence ends, just past its last byte.
    fn sequence_end(&mut self, parsed: &[u8]) -> Option<usize> {
        let parser = self.open.as_mut()?;
        let mut ends = Ends::default();
        let read = parser.advance_until_terminated(&mut ends, parsed);
        let by_callback = ends.ended.then_some(read);
        let params = if self.bytes.starts_with(b"\x1b[") {
            Some(0)
        } else if self.bytes == [ESC] && parsed.first() == Some(&b'[') {
            Some(1)
        } else {
            None
        };
        let silent = params.and_then(|from| {
            parsed[from..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map(|i| from + i + 1)
        });
        match (by_callback, silent) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Adds bytes read inside an open sequence, up to [`MAX_CARRIED`]. C0 controls there don't change the parser's
    /// state: they act straight away, or are ignored. The screen already shows their effect, so they are dropped.
    fn carry(&mut self, bytes: &[u8]) {
        let room = MAX_CARRIED.saturating_sub(self.bytes.len());
        self.bytes.extend(
            bytes
                .iter()
                .copied()
                .filter(|&b| b >= 0x20 || b == ESC)
                .take(room),
        );
    }
}

enum State {
    Ground,
    InChar,
    InSequence,
}

fn probe(bytes: &[u8]) -> State {
    let mut parser = Parser::new();
    parser.advance(&mut Ignore, bytes);
    let mut seen = Seen::default();
    parser.advance(&mut seen, &[PROBE]);
    match (seen.other, seen.prints.as_slice()) {
        (false, ['Z']) => State::Ground,
        (false, ['\u{fffd}', 'Z']) => State::InChar,
        _ => State::InSequence,
    }
}

struct Ignore;

impl Perform for Ignore {}

/// What the parser did with the probe.
#[derive(Default)]
struct Seen {
    prints: Vec<char>,
    other: bool,
}

impl Perform for Seen {
    fn print(&mut self, c: char) {
        self.prints.push(c);
    }

    fn execute(&mut self, _byte: u8) {
        self.other = true;
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.other = true;
    }

    fn put(&mut self, _byte: u8) {
        self.other = true;
    }

    fn unhook(&mut self) {
        self.other = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.other = true;
    }

    fn csi_dispatch(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        self.other = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.other = true;
    }
}

/// Stops the parser on the first callback that ends a sequence. With no ESC in the input, it is back in ground.
#[derive(Default)]
struct Ends {
    ended: bool,
}

impl Perform for Ends {
    fn print(&mut self, _c: char) {
        self.ended = true;
    }

    fn execute(&mut self, byte: u8) {
        self.ended |= byte == CAN || byte == SUB;
    }

    fn unhook(&mut self) {
        self.ended = true;
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.ended = true;
    }

    fn csi_dispatch(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _c: char) {
        self.ended = true;
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.ended = true;
    }

    fn terminated(&self) -> bool {
        self.ended
    }
}
