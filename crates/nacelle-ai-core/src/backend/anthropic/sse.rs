//! Server-sent events: the framing a turn arrives in.
//!
//! Being pedantic here pays for itself. Bytes arrive in whatever sizes
//! the kernel felt like handing over, so one frame is routinely split
//! across two reads and two frames routinely arrive in one. A parser
//! that assumed "one read is one frame" would work on a fast local
//! network and corrupt long replies over a slow one — the worst possible
//! failure schedule, because it passes every test written on a laptop.
//!
//! Only framing lives here: which bytes belong to which event, and
//! nothing about what any of it means. That belongs to
//! [`decode`](super::decode). SSE is a format rather than a provider, so
//! if a second provider ever speaks it this module moves up a level
//! unchanged.

use std::io::{ErrorKind, Read};

use crate::error::BackendError;

/// How much unparsed input may pile up before we decide the other end is
/// not speaking SSE. Without a ceiling, a server that never sends a
/// newline grows this buffer until the machine gives out — and a reply
/// body is attacker-influenced input, whatever the provider intends.
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

/// Bytes asked for per read. Frames are small; this only decides how
/// often we make a syscall.
const READ_SIZE: usize = 8 * 1024;

/// One dispatched event, reassembled: the `event:` name and the joined
/// `data:` lines.
pub(super) struct Frame {
    pub(super) name: String,
    pub(super) data: String,
}

/// The frames of one response body, pulled out as they complete.
pub(super) struct Frames<R> {
    source: R,
    /// Bytes read but not yet claimed by a complete line.
    pending: Vec<u8>,
    /// The source is exhausted. Anything still in `pending` is a partial
    /// line and will never finish.
    drained: bool,
    /// The frame being accumulated, one field at a time.
    name: String,
    data: String,
}

impl<R: Read> Frames<R> {
    pub(super) fn new(source: R) -> Self {
        Frames {
            source,
            pending: Vec::new(),
            drained: false,
            name: String::new(),
            data: String::new(),
        }
    }

    /// The next complete frame, or `None` once the body is exhausted.
    ///
    /// Blocks until a blank line arrives, because that blank line is the
    /// only thing that says a frame is whole.
    pub(super) fn next_frame(&mut self) -> Result<Option<Frame>, BackendError> {
        loop {
            match self.take_line()? {
                Some(line) => {
                    if !line.is_empty() {
                        self.field(&line)?;
                        continue;
                    }
                    // A blank line dispatches whatever accumulated.
                    // Runs of them are how some servers idle; there is
                    // nothing to dispatch for those.
                    if self.name.is_empty() && self.data.is_empty() {
                        continue;
                    }
                    return Ok(Some(Frame {
                        name: std::mem::take(&mut self.name),
                        data: std::mem::take(&mut self.data),
                    }));
                }
                None => {
                    if self.drained {
                        // A frame that never got its blank line is not a
                        // frame; the specification says to discard it.
                        // The decoder will notice the turn never ended
                        // and report that, which is the honest error —
                        // far better than handing on half a reply.
                        return Ok(None);
                    }
                    self.fill()?;
                }
            }
        }
    }

    /// One field line. Unknown fields are ignored on purpose: the
    /// provider adds them, and a reader that fell over on one would
    /// break on a Tuesday for no reason.
    fn field(&mut self, line: &str) -> Result<(), BackendError> {
        // ": keepalive" — a comment. It exists so an idle connection is
        // not reaped by a middlebox, and means nothing to us.
        if line.starts_with(':') {
            return Ok(());
        }

        let (name, value) = match line.split_once(':') {
            // Exactly one optional space after the colon is part of the
            // syntax, not of the value.
            Some((name, value)) => (name, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match name {
            "event" => {
                self.name.clear();
                self.name.push_str(value);
            }
            "data" => {
                if self.data.len() + value.len() > MAX_PENDING_BYTES {
                    return Err(BackendError::Protocol(
                        "a single stream event was larger than the ceiling for one".to_string(),
                    ));
                }
                // Several data lines are one payload split by newlines.
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            // `id` and `retry` drive resumption, which this stream does
            // not have: a turn is retried as a whole request or not at
            // all, because half a reply cannot be resumed into a whole
            // one.
            _ => {}
        }
        Ok(())
    }

    /// The next line, without its terminator, if a whole one is buffered.
    fn take_line(&mut self) -> Result<Option<String>, BackendError> {
        let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') else {
            if self.pending.len() > MAX_PENDING_BYTES {
                return Err(BackendError::Protocol(
                    "the reply had no line break where the protocol requires one".to_string(),
                ));
            }
            return Ok(None);
        };

        let mut line: Vec<u8> = self.pending.drain(..=end).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }

        // Only whole lines are decoded, so a multi-byte character split
        // across two reads is never seen half-formed here. Invalid UTF-8
        // therefore means the body really is malformed.
        String::from_utf8(line)
            .map(Some)
            .map_err(|_| BackendError::Protocol("the reply was not UTF-8".to_string()))
    }

    fn fill(&mut self) -> Result<(), BackendError> {
        let mut buffer = [0u8; READ_SIZE];
        match self.source.read(&mut buffer) {
            Ok(0) => {
                self.drained = true;
                Ok(())
            }
            Ok(read) => {
                self.pending.extend_from_slice(&buffer[..read]);
                Ok(())
            }
            // A signal interrupted the read; the connection is fine.
            Err(err) if err.kind() == ErrorKind::Interrupted => Ok(()),
            Err(err) => Err(BackendError::Network(format!(
                "the reply stopped arriving: {err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands over at most `chunk` bytes at a time, so a
    /// test can put a frame boundary wherever it likes.
    struct Chunked {
        bytes: Vec<u8>,
        at: usize,
        chunk: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let take = self.chunk.min(out.len()).min(self.bytes.len() - self.at);
            out[..take].copy_from_slice(&self.bytes[self.at..self.at + take]);
            self.at += take;
            Ok(take)
        }
    }

    fn frames(body: &str, chunk: usize) -> Vec<(String, String)> {
        let mut frames = Frames::new(Chunked {
            bytes: body.as_bytes().to_vec(),
            at: 0,
            chunk,
        });
        let mut out = Vec::new();
        while let Some(frame) = frames.next_frame().expect("framing") {
            out.push((frame.name, frame.data));
        }
        out
    }

    const BODY: &str = "event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: {}\n\n";

    #[test]
    fn the_size_of_a_read_does_not_change_the_frames() {
        // One byte at a time puts a split inside every field name, every
        // value and every terminator. Whole-body reads put two frames in
        // one buffer. Both have to come out the same.
        let whole = frames(BODY, BODY.len());
        for chunk in [1, 2, 7, 13] {
            assert_eq!(frames(BODY, chunk), whole, "chunk size {chunk}");
        }
        assert_eq!(whole.len(), 2);
        assert_eq!(
            whole[0],
            ("message_start".to_string(), "{\"a\":1}".to_string())
        );
    }

    #[test]
    fn comments_and_unknown_fields_are_skipped() {
        let body = ": keepalive\nevent: ping\nid: 7\nretry: 100\ndata: {}\n\n";
        assert_eq!(
            frames(body, 3),
            vec![("ping".to_string(), "{}".to_string())]
        );
    }

    #[test]
    fn several_data_lines_are_one_payload() {
        let body = "event: e\ndata: {\ndata: }\n\n";
        assert_eq!(frames(body, 1), vec![("e".to_string(), "{\n}".to_string())]);
    }

    #[test]
    fn carriage_returns_are_part_of_the_terminator_not_the_value() {
        let body = "event: ping\r\ndata: {}\r\n\r\n";
        assert_eq!(
            frames(body, 2),
            vec![("ping".to_string(), "{}".to_string())]
        );
    }

    #[test]
    fn a_frame_without_its_blank_line_is_discarded() {
        // The last frame here never completes. Yielding it would mean
        // handing the decoder a truncated JSON document as if it were
        // whole.
        let body = "event: a\ndata: 1\n\nevent: b\ndata: {\"partia";
        assert_eq!(frames(body, 4), vec![("a".to_string(), "1".to_string())]);
    }
}
