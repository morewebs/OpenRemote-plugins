//! The keys the driver sends to the Freebuff TUI.
//!
//! The model picker is walked one row at a time with the arrow keys, never
//! filtered by typing, so the driver never has to know whether the TUI would
//! treat a typed string as a search. A named vocabulary also keeps the escape
//! sequences in one place instead of scattered through the session as byte
//! literals that are easy to mistype.

/// A key, named for what it does rather than for the bytes it happens to be.
#[allow(dead_code)] // The catalog capture and the model walk send these next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Up,
    Down,
    Tab,
    /// Text typed into a field, never used to choose a model.
    Text(String),
}

/// The byte sequence a terminal sends for `key`.
pub fn bytes(key: Key) -> Vec<u8> {
    match key {
        // A TUI in raw mode reads a carriage return as Enter. A line feed
        // would only work in canonical mode, which is not how this runs.
        Key::Enter => b"\r".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Tab => b"\t".to_vec(),
        // Newlines are dropped so a multi-line string cannot press Enter in
        // the middle of what the caller meant as one field's worth of text.
        Key::Text(text) => text.replace(['\r', '\n'], "").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_the_sequence_a_terminal_sends() {
        assert_eq!(bytes(Key::Enter), b"\r".to_vec());
        assert_eq!(bytes(Key::Escape), b"\x1b".to_vec());
        assert_eq!(bytes(Key::Up), b"\x1b[A".to_vec());
        assert_eq!(bytes(Key::Down), b"\x1b[B".to_vec());
        assert_eq!(bytes(Key::Tab), b"\t".to_vec());
    }

    #[test]
    fn typed_text_never_carries_an_enter() {
        assert_eq!(bytes(Key::Text("a\nb\rc".into())), b"abc".to_vec());
    }
}
