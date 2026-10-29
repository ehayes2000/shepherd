//! The bytes of an escape sequence or UTF-8 character the parser has started but not finished, so a snapshot can
//! carry them and the copy's parser ends up in the same state.
//!
//! The parser keeps its state private, so it is rebuilt from what it was fed: a fresh `vte::Parser` fed these
//! bytes is in the same state as the real one. Two facts about vte make that cheap:
//! - ESC moves the parser to the escape state with cleared parameters from every state, so the bytes before the
//!   last ESC never matter.
//! - Whether a parser is back in the ground state can be asked by feeding it a probe character: only a parser in
//!   ground prints it straight away. One holding part of a UTF-8 char prints U+FFFD first.

use alacritty_terminal::vte::{Params, Parser, Perform};

const ESC: u8 = 0x1b;
/// Printed in ground, a final byte in an escape or CSI sequence, and swallowed by strings.
const PROBE: u8 = b'Z';

#[derive(Default)]
pub(crate) struct Unfinished {
    /// Starts where the parser was in ground or just read an ESC; empty while the parser is in ground.
    bytes: Vec<u8>,
}

impl Unfinished {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Follows `parsed`, the next bytes the parser consumed.
    pub(crate) fn advance(&mut self, parsed: &[u8]) {
        let parsed = match parsed.iter().rposition(|&b| b == ESC) {
            Some(esc) => {
                self.bytes.clear();
                &parsed[esc..]
            }
            None => parsed,
        };
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
        self.bytes = match probe(region) {
            State::Ground => Vec::new(),
            State::InChar => {
                // The incomplete char is the last thing fed, and nothing before it can absorb its lead byte.
                let partial = region
                    .utf8_chunks()
                    .last()
                    .map_or(0, |chunk| chunk.invalid().len());
                region[region.len() - partial..].to_vec()
            }
            // C0 controls inside a sequence don't change the parser's state: they act straight away, or are
            // ignored. The screen already shows their effect, so the copy must not run them again.
            State::InSequence => region
                .iter()
                .copied()
                .filter(|&b| b >= 0x20 || b == ESC)
                .collect(),
        };
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
