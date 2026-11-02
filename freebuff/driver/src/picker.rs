//! Reading Freebuff's model picker off the screen.
//!
//! The picker is a list of cards: a name line, then the cost line the picker
//! prints under it (`3x · Smart & Fast`), with `›` on the model the chat is
//! currently using. Nothing here knows how many rows fit on the screen: a tail
//! can be cut at the top, so every rule works from a line and its neighbours
//! alone, and never from a card's predecessor being present.
//!
//! `parse` refuses anything that does not look like the picker. A caller that
//! gets `None` has no picker to act on, and must not send a keystroke.

/// One model row.
#[allow(dead_code)] // The catalog capture and the model walk read these next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub name: String,
    /// The picker's one-line description of the model, when it prints one.
    pub tagline: Option<String>,
    /// The cost line printed under the name, kept whole because the flags
    /// after the multiplier (`2x · Recommended`) belong to the same row.
    pub detail: Option<String>,
    /// The data-use notice printed under a card, when the CLI prints one.
    pub note: Option<String>,
    /// The section header above this card, when the tail still has one.
    pub section: Option<String>,
    /// Whether this row carried the `›` marker.
    pub current: bool,
}

/// The model picker, as much of it as one tail of the screen shows.
#[allow(dead_code)] // The catalog capture and the model walk read these next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub cards: Vec<Card>,
    /// Index into `cards` of the marked row, when a marked row is visible.
    pub current: Option<usize>,
    /// The last section header above the cards.
    pub section: Option<String>,
}

/// The character the picker puts on the model the chat is using.
const CURRENT_MARKER: char = '›';
/// A marker the picker puts on a row that is not the current model.
const OTHER_MARKER: char = '×';

impl Picker {
    /// The index of the card named exactly `name`, or nothing.
    ///
    /// Matching is exact on purpose: a stale catalog naming a model the CLI
    /// has dropped must be a refusal, never a neighbouring row the user did
    /// not choose.
    #[allow(dead_code)] // The model walk asks this before pressing an arrow.
    pub fn find(&self, name: &str) -> Option<usize> {
        let wanted = name.trim();
        self.cards.iter().position(|card| card.name == wanted)
    }
}

/// Read `tail` as the model picker, or `None` when it is not one.
#[allow(dead_code)] // The catalog capture opens the picker with this.
pub fn parse(tail: &[String]) -> Option<Picker> {
    // The screen grid never hands over a blank row and a saved capture has
    // them. Dropping them here keeps every rule about "the line above" about
    // the line a person actually sees.
    let rows: Vec<&str> = tail
        .iter()
        .map(String::as_str)
        .filter(|row| !row.trim().is_empty())
        .collect();
    if rows.is_empty() {
        return None;
    }

    let detail: Vec<bool> = rows.iter().map(|row| is_detail(row)).collect();
    let marked: Vec<bool> = rows.iter().map(|row| marker(row).is_some()).collect();

    // A row is a card when a cost line follows it, when it carries a marker,
    // or when it is the first row of the tail and the tail is clearly a list.
    // The cost-line rule looks at the next row, so it is settled for the whole
    // tail before anything is built.
    let mut card: Vec<bool> = vec![false; rows.len()];
    for (index, row) in rows.iter().enumerate() {
        if detail[index] {
            continue;
        }
        let cost_line_below = index + 1 < rows.len() && detail[index + 1];
        let first_row_of_a_list = index == 0 && row.contains(" · ");
        card[index] = cost_line_below || marked[index] || first_row_of_a_list;
    }

    // A section header is a label, not a model: all capitals, and the row
    // under it is a card. An all-caps model name is a card because a cost line
    // follows it, and that is the only thing telling the two apart.
    let mut header: Vec<bool> = vec![false; rows.len()];
    for count in 0..rows.len() {
        if card[count] || detail[count] {
            continue;
        }
        header[count] = is_label_row(rows[count]) && count + 1 < rows.len() && card[count + 1];
    }

    let mut picker = Picker {
        cards: Vec::new(),
        current: None,
        section: None,
    };
    let mut section: Option<String> = None;
    // The card a following cost line or note belongs to, whether it was just
    // added or sighted a second time.
    let mut above: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        if detail[index] {
            // The cost line belongs to the card above it, and is kept verbatim
            // so the flags after the multiplier travel with it.
            if let Some(card) = above.and_then(|at| picker.cards.get_mut(at)) {
                card.detail = Some(row.trim().to_string());
            }
            continue;
        }
        if card[index] {
            let name = card_name(row);
            let at = match picker.cards.iter().position(|card| card.name == name) {
                Some(existing) => existing,
                None => {
                    picker.cards.push(Card {
                        name,
                        tagline: tagline(row),
                        detail: None,
                        note: None,
                        section: section.clone(),
                        current: false,
                    });
                    picker.cards.len() - 1
                }
            };
            // The same model can be sighted twice while the list scrolls. The
            // first sighting is the card, and a marked sighting is still the
            // answer to "where is the selection".
            if marker(row) == Some(CURRENT_MARKER) {
                picker.cards[at].current = true;
            }
            above = Some(at);
            continue;
        }
        if header[index] {
            section = Some(first_segment(row).to_string());
            continue;
        }
        // Anything else with prose in it is a note for the card above: the
        // data-use notice is printed under the card it belongs to.
        if is_note(row) {
            if let Some(card) = above.and_then(|at| picker.cards.get_mut(at)) {
                if card.note.is_none() {
                    card.note = Some(row.trim().to_string());
                }
            }
        }
    }
    picker.section = section;
    picker.current = picker.cards.iter().position(|card| card.current);

    // One card alone is not a picker. Two distinct names is the structural
    // check that keeps a transcript line with a separator from being walked.
    // Cards are deduplicated by name above, so this is a distinct count.
    if picker.cards.len() < 2 {
        return None;
    }
    Some(picker)
}

/// The first `·`-separated segment of a row, trimmed. The middle dot is the
/// picker's separator; the bullet (`•`) inside a model name is not.
fn first_segment(row: &str) -> &str {
    row.split('·').next().unwrap_or(row).trim()
}

/// The marker a row starts with, when it starts with one.
fn marker(row: &str) -> Option<char> {
    let first = row.trim_start().chars().next()?;
    matches!(first, CURRENT_MARKER | OTHER_MARKER).then_some(first)
}

/// The picker's one-line description of a model: the second segment.
fn tagline(row: &str) -> Option<String> {
    row.split('·')
        .nth(1)
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// The model name: everything before the first separator, without the marker.
fn card_name(row: &str) -> String {
    let first = first_segment(row);
    let without = match marker(row) {
        Some(marker) => first.trim_start_matches(marker),
        None => first,
    };
    without.trim().to_string()
}

/// The cost line: `1x`, `2x`, `3x`, then whatever the picker printed next.
fn is_detail(row: &str) -> bool {
    match first_segment(row).strip_suffix('x') {
        Some(digits) => !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()),
        None => false,
    }
}

/// Whether a row's first segment reads as a section label: shouting, at least
/// three characters, and nothing in it but capitals, digits, dashes and
/// spaces. An en dash counts as a dash, because Freebuff's own copy uses it
/// for ranges and a label must not stop being one over a dash character.
fn is_label_row(row: &str) -> bool {
    let label = first_segment(row);
    label.chars().count() >= 3
        && label.chars().all(|ch| {
            ch.is_uppercase() || ch.is_ascii_digit() || ch == '-' || ch == '\u{2013}' || ch == ' '
        })
}

/// Prose under a card, which the picker prints for the model's data use.
fn is_note(row: &str) -> bool {
    row.contains(' ') && row.chars().any(char::is_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The picker as a real Freebuff 0.2.35 window showed it, tail only: the
    /// first card's cost line has scrolled off the top, which is why the rules
    /// may not assume a card's predecessor is present. Copied verbatim,
    /// leading spaces and bullet characters included, because those are what
    /// the parser has to read.
    const CAPTURE: &[&str] = &[
        " Solar Mini 4 · Fast and light · Promotional",
        " May use data for AI training",
        "",
        " MiMo v2.6 Flash Slow · Slow · cheapest · Images",
        " 1x",
        "",
        " PREMIUM · 2–4x cost · lower usage (1–2 hours per day)",
        " DeepSeek V4.1 Flash Slow • high · Slow · DeepSeek · Images",
        " 2x · Recommended · May use data for AI training",
        "",
        "› DeepSeek V4.1 Flash 10/17 • high · Smart & Fast · Images",
        " 3x · May use data for AI training",
    ];

    fn capture() -> Vec<String> {
        CAPTURE.iter().map(|row| row.to_string()).collect()
    }

    #[test]
    fn the_design_capture_parses_into_four_cards_with_deepseek_marked_current() {
        let picker = parse(&capture()).expect("a picker");
        assert_eq!(picker.cards.len(), 4);
        let names: Vec<&str> = picker.cards.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Solar Mini 4",
                "MiMo v2.6 Flash Slow",
                "DeepSeek V4.1 Flash Slow • high",
                "DeepSeek V4.1 Flash 10/17 • high",
            ]
        );
        assert_eq!(picker.current, Some(3));
        assert!(picker.cards[3].current);
    }

    #[test]
    fn a_section_header_with_a_cost_range_is_not_a_card() {
        let picker = parse(&capture()).expect("a picker");
        assert!(picker.cards.iter().all(|c| c.name != "PREMIUM"));
        assert_eq!(picker.cards[3].section.as_deref(), Some("PREMIUM"));
    }

    #[test]
    fn the_data_use_line_is_a_note_not_a_card() {
        let picker = parse(&capture()).expect("a picker");
        assert!(picker
            .cards
            .iter()
            .all(|c| !c.name.starts_with("May use data")));
        // The spec's capture is a tail: the first card's detail line scrolled
        // off, so the card is still a card and the prose below it is its note.
        assert_eq!(picker.cards[0].detail, None);
        assert_eq!(
            picker.cards[0].note.as_deref(),
            Some("May use data for AI training")
        );
        assert_eq!(picker.cards[1].detail.as_deref(), Some("1x"));
    }

    #[test]
    fn an_all_caps_model_name_is_a_card_not_a_header() {
        let lines: Vec<String> = [
            "GPT-6-Codex · Fast · Images",
            "2x · Recommended",
            "PREMIUM · 2–4x cost · lower usage",
            "DeepSeek V4.1 Flash · Smart · Images",
            "3x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let picker = parse(&lines).expect("a picker");
        assert_eq!(
            picker.cards.len(),
            2,
            "an all-caps name with a cost line below it is a card"
        );
        assert_eq!(picker.cards[0].name, "GPT-6-Codex");
        assert_eq!(picker.cards[0].section, None);
        assert_eq!(picker.cards[1].section.as_deref(), Some("PREMIUM"));
    }

    #[test]
    fn a_line_without_a_detail_line_below_it_is_not_a_card() {
        let lines: Vec<String> = ["just some transcript text", "and more of it"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse(&lines).is_none());
    }

    #[test]
    fn one_card_alone_is_not_a_picker() {
        let lines: Vec<String> = ["› Only Model · Fast · Images", "2x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            parse(&lines).is_none(),
            "the spec asks for at least two distinct cards"
        );
    }

    #[test]
    fn a_marked_row_without_a_cost_line_is_still_a_card() {
        // The other marker belongs to some row that is not the current model;
        // it still proves the row is part of the list.
        let lines: Vec<String> = [
            "× Retired Model · Fast · Images",
            "DeepSeek V4.1 Flash · Smart · Images",
            "3x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let picker = parse(&lines).expect("a picker");
        assert_eq!(picker.cards.len(), 2);
        assert_eq!(picker.cards[0].name, "Retired Model");
        assert_eq!(picker.current, None);
    }

    #[test]
    fn an_unmatched_name_is_a_refusal_not_a_nearest_row() {
        let picker = parse(&capture()).expect("a picker");
        assert_eq!(picker.find("DeepSeek V4.1 Flash 10/17 • high"), Some(3));
        assert_eq!(picker.find("deepseek v4.1 flash 10/17 • high"), None);
        assert_eq!(picker.find("DeepSeek V4.1 Flash"), None);
    }
}
