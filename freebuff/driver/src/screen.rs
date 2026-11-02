//! A screen, not a log.
//!
//! The TUI repaints by rewriting rows in place, so a growing string of
//! stripped bytes holds an old copy of a row next to its replacement and a
//! matcher cannot tell which one a person is looking at. Everything that reads
//! the screen therefore reads this grid: rows of cells, a cursor, and the
//! handful of escape sequences a TUI needs to paint.
//!
//! An escape sequence this grid does not implement marks the screen
//! unparseable until the child clears it. Guessing at a half-understood
//! repaint is how a driver acts on a screen nobody is looking at, and the
//! callers here must answer "I cannot tell" instead.

/// How many recent interaction frames the diagnostic log keeps.
#[allow(dead_code)] // Used when the picker reader lands, with the grid it quotes.
pub const FRAME_LOG_CAPACITY: usize = 8;

pub struct Screen {
    rows: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// Bytes not yet consumed: an incomplete character or escape sequence.
    pending: Vec<u8>,
    unparseable: bool,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1) as usize;
        let cols = cols.max(1) as usize;
        Self {
            rows: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            pending: Vec::new(),
            unparseable: false,
        }
    }

    #[allow(dead_code)] // The picker reader walks the size it is given.
    pub fn size(&self) -> (u16, u16) {
        (self.rows.len() as u16, self.rows[0].len() as u16)
    }

    fn height(&self) -> usize {
        self.rows.len()
    }

    fn width(&self) -> usize {
        self.rows[0].len()
    }

    /// Parse everything that is complete in `pending`, leaving the rest for the
    /// next read. A character or escape sequence can be split across reads.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut i = 0;
        while i < self.pending.len() {
            let byte = self.pending[i];
            if byte == 0x1b {
                match self.escape(i) {
                    Some(next) => i = next,
                    None => break,
                }
            } else if byte == b'\r' {
                self.col = 0;
                i += 1;
            } else if byte == b'\n' {
                self.newline();
                i += 1;
            } else if byte == b'\t' {
                let next = ((self.col / 8) + 1) * 8;
                self.col = next.min(self.width().saturating_sub(1));
                i += 1;
            } else if byte < 0x20 || byte == 0x7f {
                i += 1;
            } else {
                match std::str::from_utf8(&self.pending[i..]) {
                    Ok(text) => {
                        let ch = text.chars().next().unwrap_or(' ');
                        self.put(ch);
                        i += ch.len_utf8();
                    }
                    Err(err) => {
                        let valid = err.valid_up_to();
                        if valid == 0 {
                            // A character split across reads: wait for the rest.
                            break;
                        }
                        if let Ok(text) = std::str::from_utf8(&self.pending[i..i + valid]) {
                            if let Some(ch) = text.chars().next() {
                                self.put(ch);
                                i += ch.len_utf8();
                                continue;
                            }
                        }
                        self.unparseable = true;
                        i += valid;
                    }
                }
            }
        }
        self.pending.drain(..i);
    }

    fn put(&mut self, ch: char) {
        if self.col >= self.width() {
            self.col = 0;
            self.newline();
        }
        let row = self.row.min(self.height() - 1);
        self.rows[row][self.col] = ch;
        self.col += 1;
    }

    fn newline(&mut self) {
        if self.row + 1 >= self.height() {
            self.rows.remove(0);
            self.rows.push(vec![' '; self.width()]);
            self.row = self.height() - 1;
        } else {
            self.row += 1;
        }
    }

    fn clear(&mut self) {
        for row in &mut self.rows {
            row.iter_mut().for_each(|cell| *cell = ' ');
        }
        self.unparseable = false;
    }

    /// The last `lines` rows that carry text, oldest first. `None` when the
    /// screen has seen an escape sequence this grid does not implement since
    /// its last clear: the caller must not guess.
    pub fn tail(&self, lines: usize) -> Option<Vec<String>> {
        if self.unparseable {
            return None;
        }
        let mut out: Vec<String> = Vec::new();
        for row in self.rows.iter().rev() {
            let text: String = row.iter().collect();
            let text = text.trim_end().to_string();
            if text.is_empty() {
                continue;
            }
            out.push(text);
            if out.len() == lines {
                break;
            }
        }
        out.reverse();
        Some(out)
    }

    /// The tail as one string for the substring matchers, rows joined the way
    /// the diagnostics already quote them.
    pub fn joined_tail(&self, lines: usize) -> Option<String> {
        self.tail(lines).map(|rows| rows.join(" | "))
    }

    /// One escape sequence starting at `i`, returning the index after it, or
    /// `None` when the bytes so far are not enough to decide.
    fn escape(&mut self, i: usize) -> Option<usize> {
        let mut j = i + 1;
        if j >= self.pending.len() {
            return None;
        }
        match self.pending[j] {
            b'[' => {
                j += 1;
                let start = j;
                while j < self.pending.len() && matches!(self.pending[j], 0x20..=0x3f) {
                    j += 1;
                }
                if j >= self.pending.len() {
                    return None;
                }
                let final_byte = self.pending[j];
                if !matches!(final_byte, 0x40..=0x7e) {
                    // Malformed: consume what we read and stop trusting the screen.
                    self.unparseable = true;
                    return Some(j + 1);
                }
                let params = String::from_utf8_lossy(&self.pending[start..j]).to_string();
                self.apply_csi(&params, final_byte);
                Some(j + 1)
            }
            b']' => {
                // OSC: skip to BEL or ST (ESC \).
                j += 1;
                while j < self.pending.len() {
                    if self.pending[j] == 0x07 {
                        return Some(j + 1);
                    }
                    if self.pending[j] == 0x1b {
                        if j + 1 >= self.pending.len() {
                            return None;
                        }
                        if self.pending[j + 1] == b'\\' {
                            return Some(j + 2);
                        }
                    }
                    j += 1;
                }
                None
            }
            _ => {
                // ESC with any other introducer is not a form this grid paints.
                self.unparseable = true;
                Some(j + 1)
            }
        }
    }

    fn apply_csi(&mut self, params: &str, final_byte: u8) {
        let private = params.starts_with('?');
        let body = if private { &params[1..] } else { params };
        let numbers: Vec<Option<usize>> = if body.is_empty() {
            vec![None]
        } else {
            body.split(';')
                .map(|part| part.parse::<usize>().ok())
                .collect()
        };
        let n = |default: usize| {
            numbers
                .first()
                .copied()
                .flatten()
                .filter(|value| *value > 0)
                .unwrap_or(default)
        };

        if private {
            match final_byte {
                // Cursor visibility, bracketed paste, focus reporting,
                // synchronized output, grapheme width, theme notifications
                // and sixel scrolling: each one tells the terminal how to
                // report input or when to present a frame, and none of them
                // changes a cell. Freebuff's TUI brackets every repaint in
                // synchronized output, so without this the grid would refuse
                // a screen that is perfectly readable.
                b'h' | b'l' if ignorable_mode(body) => {}
                // A mode query (`CSI ? 2004 $ p`) asks the terminal what it
                // supports; the answer goes out on the write side, not here.
                b'p' if body.ends_with('$') => {}
                // `CSI ? u` asks which keyboard protocol the terminal speaks.
                b'u' if body.is_empty() => {}
                _ => self.unparseable = true,
            }
            return;
        }

        match final_byte {
            b'A' => self.row = self.row.saturating_sub(n(1)),
            b'B' => self.row = (self.row + n(1)).min(self.height() - 1),
            b'C' => self.col = (self.col + n(1)).min(self.width().saturating_sub(1)),
            b'D' => self.col = self.col.saturating_sub(n(1)),
            b'H' | b'f' => {
                let row = numbers.first().copied().flatten().unwrap_or(1).max(1);
                let col = numbers.get(1).copied().flatten().unwrap_or(1).max(1);
                self.row = (row - 1).min(self.height() - 1);
                self.col = (col - 1).min(self.width().saturating_sub(1));
            }
            b'G' => self.col = (n(1) - 1).min(self.width().saturating_sub(1)),
            b'd' => self.row = (n(1) - 1).min(self.height() - 1),
            // Erase characters: the TUI blanks the tail of a row this way
            // instead of writing spaces, and the cursor stays put.
            b'X' => {
                let row = self.row.min(self.height() - 1);
                let end = (self.col + n(1)).min(self.width());
                for col in self.col..end {
                    self.rows[row][col] = ' ';
                }
            }
            b'J' => match numbers.first().copied().flatten().unwrap_or(0) {
                2 | 3 => {
                    self.clear();
                    self.row = 0;
                    self.col = 0;
                }
                0 => self.erase_to_end_of_screen(),
                1 => {
                    for row in 0..self.row {
                        self.rows[row].iter_mut().for_each(|cell| *cell = ' ');
                    }
                    self.erase_line(1);
                }
                _ => self.unparseable = true,
            },
            b'K' => self.erase_line(numbers.first().copied().flatten().unwrap_or(0)),
            b'm' => {}
            b'S' => {
                for _ in 0..n(1) {
                    self.rows.remove(0);
                    self.rows.push(vec![' '; self.width()]);
                }
            }
            b'T' => {
                for _ in 0..n(1) {
                    self.rows.pop();
                    self.rows.insert(0, vec![' '; self.width()]);
                }
            }
            // A cursor-position report is a query, not paint: the driver
            // answers it on the write side.
            b'n' => {
                if numbers.first().copied().flatten() != Some(6) {
                    self.unparseable = true;
                }
            }
            // Window and pixel-size reports are queries too (`CSI 14 t` asks
            // for the text area in pixels). A resize request would be a real
            // layout change and is not something this grid follows.
            b't' => {
                if !matches!(numbers.first().copied().flatten(), Some(14) | Some(18)) {
                    self.unparseable = true;
                }
            }
            // A mode query without the private prefix: the answer goes out on
            // the write side.
            b'p' => {
                if !params.ends_with('$') {
                    self.unparseable = true;
                }
            }
            // DECSCUSR sets the cursor shape (`CSI > 0 q`); no cell changes.
            b'q' => {
                if !params.starts_with('>') {
                    self.unparseable = true;
                }
            }
            _ => self.unparseable = true,
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row = self.row.min(self.height() - 1);
        match mode {
            0 => {
                for col in self.col..self.width() {
                    self.rows[row][col] = ' ';
                }
            }
            1 => {
                for col in 0..=self.col.min(self.width() - 1) {
                    self.rows[row][col] = ' ';
                }
            }
            2 => self.rows[row].iter_mut().for_each(|cell| *cell = ' '),
            _ => self.unparseable = true,
        }
    }

    fn erase_to_end_of_screen(&mut self) {
        self.erase_line(0);
        for row in (self.row + 1)..self.height() {
            self.rows[row].iter_mut().for_each(|cell| *cell = ' ');
        }
    }
}

/// Private modes that change no cell: they tell the terminal how to report
/// input or when to present a frame, so a screen that switched them on is
/// still the screen a person sees.
///
/// The alternate-screen modes are deliberately absent. Switching buffers
/// replaces everything on screen, and this grid follows one screen, so a
/// buffer switch must keep the "I cannot tell" answer.
fn ignorable_mode(body: &str) -> bool {
    body.split(';').all(|mode| {
        matches!(
            mode,
            // Cursor visible/hidden, focus reporting, sixel scrolling,
            // bracketed paste, synchronized output, grapheme width, and
            // light/dark theme notifications.
            "25" | "1004" | "1016" | "2004" | "2026" | "2027" | "2031"
        )
    })
}

/// The last few screens the driver acted on, kept so a parse that breaks can
/// be reported with the screen it broke on. Quoted on stderr only: a frame can
/// carry balance text or an account address, and nothing here leaves the
/// machine.
#[allow(dead_code)] // Wired into the picker reader; the frames it keeps are the point.
pub struct FrameLog {
    entries: Vec<(String, String)>,
    capacity: usize,
}

#[allow(dead_code)]
impl FrameLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, label: &str, tail: String) {
        if self.entries.len() == self.capacity {
            self.entries.remove(0);
        }
        self.entries.push((label.to_string(), tail));
    }

    pub fn last(&self) -> Option<(&str, &str)> {
        self.entries
            .last()
            .map(|(label, tail)| (label.as_str(), tail.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_rewrite_replaces_the_old_row_not_append() {
        let mut screen = Screen::new(45, 140);
        screen.feed(b"one\r\ntwo");
        // Rewrite the row the cursor is on, the way a TUI repaints a line.
        screen.feed(b"\r\x1b[2Ktwo-changed");
        assert_eq!(
            screen.tail(2),
            Some(vec!["one".into(), "two-changed".into()])
        );
    }

    #[test]
    fn erasing_the_screen_empties_the_tail() {
        let mut screen = Screen::new(45, 140);
        screen.feed(b"a\r\nb\x1b[2J\x1b[Hc");
        assert_eq!(screen.tail(3), Some(vec!["c".into()]));
    }

    #[test]
    fn a_newline_at_the_bottom_scrolls_and_keeps_the_last_frame() {
        let mut screen = Screen::new(3, 20);
        screen.feed(b"1\r\n2\r\n3\r\n4");
        assert_eq!(
            screen.tail(3),
            Some(vec!["2".into(), "3".into(), "4".into()])
        );
    }

    #[test]
    fn an_unknown_escape_sequence_marks_the_screen_unparseable() {
        let mut screen = Screen::new(45, 140);
        screen.feed(b"models here\x1b[?1049h");
        assert_eq!(screen.tail(10), None);
        // A clear brings it back: the screen is again something a person sees.
        screen.feed(b"\x1b[2J\x1b[Hfresh");
        assert_eq!(screen.tail(10), Some(vec!["fresh".into()]));
    }

    #[test]
    fn a_multibyte_char_split_across_reads_still_lands() {
        let mut screen = Screen::new(45, 140);
        let bytes = "· ok".as_bytes();
        screen.feed(&bytes[..1]);
        screen.feed(&bytes[1..]);
        assert_eq!(screen.tail(1), Some(vec!["· ok".into()]));
    }

    #[test]
    fn the_tail_skips_blank_rows_and_keeps_order() {
        let mut screen = Screen::new(45, 140);
        screen.feed(b"a\r\n\r\nb\r\n\r\nc");
        assert_eq!(screen.tail(2), Some(vec!["b".into(), "c".into()]));
    }

    #[test]
    fn a_wrapped_long_line_keeps_reading_order() {
        let mut screen = Screen::new(45, 10);
        screen.feed(b"0123456789ABCDEF");
        assert_eq!(
            screen.tail(2),
            Some(vec!["0123456789".into(), "ABCDEF".into()])
        );
    }

    #[test]
    fn the_frame_log_keeps_the_last_eight_interactions() {
        let mut log = FrameLog::new(8);
        for i in 0..12 {
            log.push(&format!("step {i}"), format!("screen {i}"));
        }
        let (label, tail) = log.last().expect("a frame");
        assert_eq!(label, "step 11");
        assert_eq!(tail, "screen 11");
    }

    #[test]
    fn an_escape_split_across_reads_is_not_lost() {
        let mut screen = Screen::new(10, 20);
        screen.feed(b"old\x1b[");
        screen.feed(b"2J\x1b[Hnew");
        assert_eq!(screen.tail(1), Some(vec!["new".into()]));
    }

    #[test]
    fn an_erase_in_line_clears_only_the_row_it_is_on() {
        let mut screen = Screen::new(10, 20);
        screen.feed(b"keep\r\nwipe\x1b[2K\r\nnext");
        assert_eq!(screen.tail(3), Some(vec!["keep".into(), "next".into()]));
    }

    #[test]
    fn erased_characters_are_blanked_without_moving_the_cursor() {
        let mut screen = Screen::new(5, 20);
        screen.feed(b"abcdef\x1b[4D\x1b[2X");
        assert_eq!(screen.tail(1), Some(vec!["ab  ef".into()]));
    }

    #[test]
    fn the_terminals_mode_and_query_sequences_do_not_blind_the_grid() {
        // Measured on Freebuff 0.2.35, whose TUI switches synchronized output
        // on and off around every repaint and queries its modes before it
        // paints. None of these change a cell, so the screen stays readable.
        let mut screen = Screen::new(45, 140);
        screen.feed(b"\x1b[?2026h\x1b[?2027h\x1b[?2004h\x1b[?1004h\x1b[?2031h");
        screen.feed(b"\x1b[?2004$p\x1b[?2026$p\x1b[?u\x1b[>0q\x1b[14t");
        screen.feed(b"models\x1b[?2026l\x1b[m");
        assert_eq!(screen.tail(1), Some(vec!["models".into()]));
    }

    #[test]
    fn an_alternate_screen_still_leaves_the_grid_unable_to_tell() {
        // Switching buffers replaces the whole screen, so the rows after it
        // are not a continuation of the rows before it.
        let mut screen = Screen::new(45, 140);
        screen.feed(b"before\x1b[?1049h");
        assert_eq!(screen.tail(1), None);
    }
}
