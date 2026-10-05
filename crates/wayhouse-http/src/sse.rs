//! Reassembles Server-Sent Events from a response body that arrives in
//! arbitrary chunks, for every fleet client that follows a `/…/subscribe`
//! stream.
//!
//! The buffer holds only the **unterminated** tail: complete events are handed
//! out by [`EventBuffer::next_event`] and dropped. A peer that never sends the
//! blank line that ends an event would otherwise grow it without bound, so a
//! tail past [`MAX_EVENT_BYTES`] is an error and the caller drops the stream
//! (each subscriber reconnects with backoff). Bytes are buffered raw and
//! decoded once per complete event, so a multi-byte character split across two
//! chunks survives.

/// Largest single event (one config revision, or one registry snapshot) a
/// subscriber accepts. Generous next to any real payload; its only job is to
/// bound memory against a misbehaving or hostile peer.
pub const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// An event grew past the limit without terminating.
#[derive(Debug, thiserror::Error)]
#[error("an SSE event exceeded {limit} bytes without ending")]
pub struct EventTooLarge {
    pub limit: usize,
}

pub struct EventBuffer {
    buf: Vec<u8>,
    /// Bytes after the last `\n\n` seen so far: the event still being received.
    tail_len: usize,
    limit: usize,
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBuffer {
    pub fn new() -> Self {
        Self::with_limit(MAX_EVENT_BYTES)
    }

    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            tail_len: 0,
            limit,
        }
    }

    /// Appends a chunk. Errors, leaving the buffer unusable, when the event in
    /// progress is longer than the limit.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), EventTooLarge> {
        // A terminator may straddle the previous chunk and this one.
        let from = self.buf.len().saturating_sub(1);
        self.buf.extend_from_slice(chunk);
        match self.buf[from..].windows(2).rposition(|w| w == b"\n\n") {
            Some(i) => self.tail_len = self.buf.len() - (from + i + 2),
            None => self.tail_len += chunk.len(),
        }
        if self.tail_len > self.limit {
            return Err(EventTooLarge { limit: self.limit });
        }
        Ok(())
    }

    /// The next complete event block (without its terminating blank line).
    pub fn next_event(&mut self) -> Option<String> {
        let end = self.buf.windows(2).position(|w| w == b"\n\n")?;
        let event = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end + 2);
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(b: &mut EventBuffer) -> Vec<String> {
        std::iter::from_fn(|| b.next_event()).collect()
    }

    #[test]
    fn splits_events_across_and_within_chunks() {
        let mut b = EventBuffer::new();
        b.push(b"data: a\n\ndata: b\n").unwrap();
        assert_eq!(drain(&mut b), ["data: a"]);
        b.push(b"\n: keep-alive\n\n").unwrap();
        assert_eq!(drain(&mut b), ["data: b", ": keep-alive"]);
    }

    #[test]
    fn a_terminator_split_between_chunks_is_found() {
        let mut b = EventBuffer::new();
        b.push(b"data: a\n").unwrap();
        assert!(b.next_event().is_none());
        b.push(b"\n").unwrap();
        assert_eq!(drain(&mut b), ["data: a"]);
    }

    #[test]
    fn a_multibyte_character_split_between_chunks_survives() {
        let bytes = "data: é\n\n".as_bytes();
        let mut b = EventBuffer::new();
        b.push(&bytes[..7]).unwrap(); // mid-"é"
        b.push(&bytes[7..]).unwrap();
        assert_eq!(drain(&mut b), ["data: é"]);
    }

    #[test]
    fn an_unterminated_event_past_the_limit_is_an_error() {
        let mut b = EventBuffer::with_limit(16);
        b.push(&[b'x'; 10]).unwrap();
        assert!(b.push(&[b'x'; 10]).is_err());
    }

    #[test]
    fn many_complete_events_in_one_chunk_are_not_an_overflow() {
        let mut b = EventBuffer::with_limit(16);
        let chunk = b"data: 1\n\n".repeat(50);
        b.push(&chunk).unwrap();
        assert_eq!(drain(&mut b).len(), 50);
        // The tail resets once events end: a fresh event fits again.
        b.push(b"data: 2\n\n").unwrap();
    }

    #[test]
    fn the_limit_counts_only_the_unterminated_tail() {
        let mut b = EventBuffer::with_limit(16);
        b.push(b"data: 1\n\nxxxxxxxxxx").unwrap(); // 10-byte tail
        assert!(b.push(b"xxxxxxx").is_err()); // 17
    }
}
