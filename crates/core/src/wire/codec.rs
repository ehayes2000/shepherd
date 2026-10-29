//! Newline-delimited framing: one JSON message per line.

/// The longest line a [`Decoder`] accepts by default, not counting the line ending.
pub const MAX_LINE: usize = 16 * 1024 * 1024;

/// A line was longer than the decoder's limit. The decoder dropped it and resumes at the next line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line exceeds {max} bytes")]
pub struct LineTooLong {
    pub max: usize,
}

/// Splits a byte stream into lines. Feed it chunks as they arrive with [`Decoder::push`], then drain whole
/// lines with [`Decoder::next_line`]. Lines end in `\n` or `\r\n`, the ending is stripped, and empty lines are
/// skipped.
#[derive(Debug, Clone)]
pub struct Decoder {
    buf: Vec<u8>,
    max: usize,
    /// How much of `buf` is known to hold no `\n`.
    scanned: usize,
    /// Dropping the rest of a line that went over the limit.
    discarding: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::with_max_line(MAX_LINE)
    }

    pub fn with_max_line(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
            scanned: 0,
            discarding: false,
        }
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        if self.discarding {
            let Some(i) = chunk.iter().position(|&b| b == b'\n') else {
                return;
            };
            self.discarding = false;
            chunk = &chunk[i + 1..];
        }
        self.buf.extend_from_slice(chunk);
    }

    /// The next whole line, or `None` until more bytes arrive. A line over the limit is reported once as an
    /// error, as soon as the buffer passes the limit, and the rest of it is dropped.
    pub fn next_line(&mut self) -> Option<Result<Vec<u8>, LineTooLong>> {
        let too_long = Err(LineTooLong { max: self.max });
        loop {
            let Some(i) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') else {
                self.scanned = self.buf.len();
                // One extra byte may be the `\r` of a line that is exactly at the limit.
                let over = self.buf.len() > self.max + 1
                    || (self.buf.len() == self.max + 1 && self.buf.last() != Some(&b'\r'));
                if over {
                    self.buf.clear();
                    self.scanned = 0;
                    self.discarding = true;
                    return Some(too_long);
                }
                return None;
            };
            let mut line: Vec<u8> = self.buf.drain(..=self.scanned + i).collect();
            self.scanned = 0;
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > self.max {
                return Some(too_long);
            }
            if !line.is_empty() {
                return Some(Ok(line));
            }
        }
    }
}

/// Appends the line ending to an encoded message.
pub(crate) fn frame(mut json: Vec<u8>) -> Vec<u8> {
    json.push(b'\n');
    json
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(d: &mut Decoder) -> Vec<Result<Vec<u8>, LineTooLong>> {
        std::iter::from_fn(|| d.next_line()).collect()
    }

    fn lines(d: &mut Decoder) -> Vec<String> {
        drain(d)
            .into_iter()
            .map(|l| String::from_utf8(l.unwrap()).unwrap())
            .collect()
    }

    const STREAM: &[u8] = b"{\"a\":1}\n{\"b\":\"two\"}\r\n{\"c\":[3]}\n";

    #[test]
    fn yields_each_whole_line_from_one_chunk() {
        let mut d = Decoder::new();
        d.push(STREAM);
        assert_eq!(
            lines(&mut d),
            [r#"{"a":1}"#, r#"{"b":"two"}"#, r#"{"c":[3]}"#]
        );
    }

    #[test]
    fn splitting_at_every_byte_boundary_yields_the_same_lines() {
        for split in 0..=STREAM.len() {
            let mut d = Decoder::new();
            let mut got = Vec::new();
            for part in [&STREAM[..split], &STREAM[split..]] {
                d.push(part);
                got.extend(lines(&mut d));
            }
            assert_eq!(
                got,
                [r#"{"a":1}"#, r#"{"b":"two"}"#, r#"{"c":[3]}"#],
                "split at {split}"
            );
        }
    }

    #[test]
    fn byte_at_a_time_yields_the_same_lines() {
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for b in STREAM {
            d.push(std::slice::from_ref(b));
            got.extend(lines(&mut d));
        }
        assert_eq!(got, [r#"{"a":1}"#, r#"{"b":"two"}"#, r#"{"c":[3]}"#]);
    }

    #[test]
    fn partial_line_is_held_until_its_newline_arrives() {
        let mut d = Decoder::new();
        d.push(b"{\"a\":");
        assert!(d.next_line().is_none());
        d.push(b"1}\n");
        assert_eq!(lines(&mut d), [r#"{"a":1}"#]);
    }

    #[test]
    fn empty_lines_are_skipped() {
        let mut d = Decoder::new();
        d.push(b"\n\r\n{}\n\n\r\n[]\n");
        assert_eq!(lines(&mut d), ["{}", "[]"]);
    }

    #[test]
    fn line_longer_than_max_is_an_error_without_buffering_it() {
        let mut d = Decoder::with_max_line(4);
        d.push(b"12345");
        assert_eq!(drain(&mut d), [Err(LineTooLong { max: 4 })]);
        d.push(b"6789");
        assert!(drain(&mut d).is_empty(), "rest of the long line is dropped");
        assert!(d.buf.len() <= 4, "decoder must not grow past its limit");
    }

    #[test]
    fn decoder_resumes_at_the_line_after_a_long_one() {
        let mut d = Decoder::with_max_line(4);
        d.push(b"123456789\nok\n");
        assert_eq!(
            drain(&mut d),
            [Err(LineTooLong { max: 4 }), Ok(b"ok".to_vec())]
        );
    }

    #[test]
    fn line_of_exactly_max_bytes_is_accepted() {
        let mut d = Decoder::with_max_line(4);
        d.push(b"1234\r\n");
        assert_eq!(lines(&mut d), ["1234"]);
        for b in b"5678\r\n" {
            d.push(std::slice::from_ref(b));
            assert!(!drain(&mut d).iter().any(Result::is_err));
        }
    }
}
