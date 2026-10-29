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
//! An open sequence is carried up to [`MAX_CARRIED`] bytes. Past that only the bytes that move it to another part of
//! vte's state machine are kept ([`Part`]), so the copy still ends the sequence on the same byte.
//!
//! vte handles invalid UTF-8 differently depending on where the input was split, and the replay reads its bytes in
//! one piece, so for invalid input the copy follows the unsplit result. Valid UTF-8 is unaffected.

use alacritty_terminal::vte::{Params, Parser, Perform};

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
/// Printed in ground, a final byte in an escape or CSI sequence, and swallowed by strings.
const PROBE: u8 = b'Z';
/// The most bytes of one open sequence that are carried in full. Past this the copy still ends the sequence on the
/// same byte, but what the sequence does then may differ: an OSC is dispatched with its payload cut short (shepherd
/// ignores titles and the clipboard, and an OSC 4 colour list is applied only as far as the cut), and a CSI or
/// escape sequence is dispatched with the parameters it had at the cut. alacritty ignores DCS, SOS, PM and APC
/// payloads anyway.
const MAX_CARRIED: usize = 64 * 1024;

pub(crate) struct Unfinished {
    /// Starts at an ESC or at the lead byte of a partial char; empty while the parser is in ground.
    bytes: Vec<u8>,
    /// While a sequence is open: a parser that has read all of it so far, to find where it ends, and the part of the
    /// state machine it is in.
    open: Option<(Parser, Part)>,
    cap: usize,
    /// Bytes fed to the parsers here, for tests that check the work stays linear.
    work: u64,
}

impl Default for Unfinished {
    fn default() -> Self {
        Self::with_cap(MAX_CARRIED)
    }
}

impl Unfinished {
    fn with_cap(cap: usize) -> Self {
        Self {
            bytes: Vec::new(),
            open: None,
            cap,
            work: 0,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn work(&self) -> u64 {
        self.work
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
        self.work += region.len() as u64 + 1;
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
                // Only an ESC leaves ground, so the open sequence starts at the region's first byte.
                let mut parser = Parser::new();
                parser.advance(&mut Ignore, region);
                self.work += region.len() as u64;
                self.open = Some((parser, Part::Escape));
                self.bytes = vec![ESC];
                self.carry(&region[1..]);
            }
        }
    }

    /// Where in `parsed` (with no ESC) the open sequence ends, just past its last byte.
    fn sequence_end(&mut self, parsed: &[u8]) -> Option<usize> {
        let (parser, part) = self.open.as_mut()?;
        let mut ends = Ends::default();
        let read = parser.advance_until_terminated(&mut ends, parsed);
        self.work += read as u64;
        let by_callback = ends.ended.then_some(read);
        let mut part = *part;
        let silent = parsed.iter().position(|&b| {
            let ends = matches!(part, Part::Csi(_)) && (0x40..=0x7e).contains(&b);
            part = part.next(b);
            ends
        });
        match (by_callback, silent.map(|i| i + 1)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Adds bytes read inside an open sequence. C0 controls there don't change the parser's state: they act straight
    /// away, or are ignored. The screen already shows their effect, so they are dropped. Past the cap, only bytes
    /// that move the sequence to another [`Part`] are kept.
    fn carry(&mut self, bytes: &[u8]) {
        let Some((_, part)) = self.open.as_mut() else {
            return;
        };
        for &b in bytes {
            if b < 0x20 {
                continue;
            }
            let next = part.next(b);
            if self.bytes.len() < self.cap || next != *part {
                self.bytes.push(b);
            }
            *part = next;
        }
    }
}

/// The part of vte's state machine an open sequence is in, as far as the bytes that don't end it can move it. This
/// mirrors vte 0.15's transitions; the unit tests check it against vte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Escape,
    EscapeIntermediate,
    Csi(Header),
    Dcs(Header),
    DcsPassthrough,
    /// OSC, SOS, PM or APC.
    String,
}

/// Where a CSI or a DCS is before its final byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Header {
    Entry,
    Param,
    Intermediate,
    Ignore,
}

impl Part {
    fn next(self, b: u8) -> Part {
        match (self, b) {
            (Part::Escape, 0x20..=0x2f) => Part::EscapeIntermediate,
            (Part::Escape, b'[') => Part::Csi(Header::Entry),
            (Part::Escape, b'P') => Part::Dcs(Header::Entry),
            (Part::Escape, b']' | b'X' | b'^' | b'_') => Part::String,
            (Part::Csi(header), _) => Part::Csi(header.next(b)),
            (Part::Dcs(header), 0x40..=0x7e) if header != Header::Ignore => Part::DcsPassthrough,
            (Part::Dcs(header), _) => Part::Dcs(header.next(b)),
            _ => self,
        }
    }
}

impl Header {
    fn next(self, b: u8) -> Header {
        match (self, b) {
            (Header::Entry | Header::Param, 0x20..=0x2f) => Header::Intermediate,
            (Header::Entry, 0x30..=0x3f) => Header::Param,
            (Header::Param, 0x3c..=0x3f) | (Header::Intermediate, 0x30..=0x3f) => Header::Ignore,
            _ => self,
        }
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// What a parser does, without parameter values (which may differ past the cap).
    #[derive(Debug, PartialEq)]
    enum Step {
        Print(char),
        Execute(u8),
        Hook(char),
        Put(u8),
        Unhook,
        Osc,
        Csi(char),
        Esc(u8),
    }

    #[derive(Default)]
    struct Trace(Vec<Step>);

    impl Perform for Trace {
        fn print(&mut self, c: char) {
            self.0.push(Step::Print(c));
        }

        fn execute(&mut self, byte: u8) {
            self.0.push(Step::Execute(byte));
        }

        fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
            self.0.push(Step::Hook(action));
        }

        fn put(&mut self, byte: u8) {
            self.0.push(Step::Put(byte));
        }

        fn unhook(&mut self) {
            self.0.push(Step::Unhook);
        }

        fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
            self.0.push(Step::Osc);
        }

        fn csi_dispatch(
            &mut self,
            _params: &Params,
            _intermediates: &[u8],
            _ignore: bool,
            c: char,
        ) {
            self.0.push(Step::Csi(c));
        }

        fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
            self.0.push(Step::Esc(byte));
        }
    }

    fn trace_after(prefix: &[u8], rest: &[u8]) -> Vec<Step> {
        let mut parser = Parser::new();
        parser.advance(&mut Ignore, prefix);
        let mut trace = Trace::default();
        parser.advance(&mut trace, rest);
        trace.0
    }

    /// Bytes from every class vte's sequence states tell apart, but no ESC.
    const ALPHABET: &[u8] = b" !/09:;<=?@P[]q~\x7f\x80\x9c\n\x18";

    proptest! {
        #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

        /// With a tiny cap, the carried bytes leave a fresh parser reacting to any next byte as the real one does.
        #[test]
        fn carried_bytes_past_the_cap_end_the_sequence_like_the_original(
            intro in prop::sample::select(vec![&b"\x1b"[..], b"\x1b[", b"\x1bP", b"\x1b]", b"\x1b_"]),
            body in prop::collection::vec(prop::sample::select(ALPHABET), 0..24),
            split in any::<prop::sample::Index>(),
        ) {
            let fed: Vec<u8> = [intro, &body[..]].concat();
            let split = split.index(fed.len() + 1);
            let mut unfinished = Unfinished::with_cap(3);
            unfinished.advance(&fed[..split]);
            unfinished.advance(&fed[split..]);
            if unfinished.open.is_none() {
                return Ok(());
            }
            for next in (0x18..=0x7f).chain([0x9c]) {
                let rest = [next, b'Z', b'z'];
                prop_assert_eq!(
                    trace_after(unfinished.bytes(), &rest),
                    trace_after(&fed, &rest),
                    "fed {:?}, carried {:?}, then {:?}",
                    fed,
                    unfinished.bytes(),
                    rest
                );
            }
        }
    }
}
