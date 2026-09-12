//! The one-line editor behind `/`.
//!
//! Lifted from ticket-tui, trimmed to what a search box uses: its wrapping,
//! its newlines and its block paste stayed behind with the multi-line body
//! editors that wanted them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single-line text field: the text plus a caret measured in characters,
/// with the editing behaviour the search box on every table shares. Each
/// table owns one, which is why the field is a value rather than a global:
/// going down into a repository and back out again finds what was typed
/// still there.
// ponytail: the caret counts characters, not grapheme clusters, so a
// backspace over a combining mark takes the mark and leaves its base. Reach
// for the `unicode-segmentation` crate if search queries ever carry composed
// text rather than the names of Azure resources.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    /// Creates a field holding `text` with the caret at the end.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.chars().count();
        Self { text, cursor }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text and moves the caret to the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        *self = Self::new(text);
    }

    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.character_count());
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_char(&mut self, character: char) {
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert(byte, character);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let byte = byte_index(&self.text, self.cursor);
        self.text.insert_str(byte, text);
        self.cursor += text.chars().count();
    }

    /// Deletes the character before the caret, reporting whether it removed one.
    pub fn backspace(&mut self) -> bool {
        let Some(index) = self.cursor.checked_sub(1) else {
            return false;
        };
        self.remove_range(index, index + 1);
        self.cursor = index;
        true
    }

    /// Deletes the character under the caret, reporting whether it removed one.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.character_count() {
            return false;
        }
        self.remove_range(self.cursor, self.cursor + 1);
        true
    }

    /// Deletes the whitespace before the caret and the word before that,
    /// reporting whether it removed anything.
    pub fn delete_word(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.word_start();
        self.remove_range(start, self.cursor);
        self.cursor = start;
        true
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = self.cursor.saturating_add(1).min(self.character_count());
    }

    /// Moves the caret to the front of the word before it, which is where
    /// [`Self::delete_word`] would have cut.
    pub fn move_word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// Moves the caret past the whitespace under it and the word after that,
    /// the mirror of [`Self::move_word_left`].
    pub fn move_word_right(&mut self) {
        let characters: Vec<char> = self.text.chars().collect();
        let mut end = self.cursor;
        while end < characters.len() && characters[end].is_whitespace() {
            end += 1;
        }
        while end < characters.len() && !characters[end].is_whitespace() {
            end += 1;
        }
        self.cursor = end;
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.character_count();
    }

    /// Inserts pasted text at the caret. The field holds one logical line of
    /// query text, so a pasted newline or tab folds into a space rather than
    /// joining two words into one, and the other control characters are
    /// dropped.
    pub fn paste(&mut self, pasted: &str) {
        let sanitized: String = pasted
            .chars()
            .filter_map(|character| match character {
                '\r' | '\n' | '\t' => Some(' '),
                character if character.is_control() => None,
                character => Some(character),
            })
            .collect();
        self.insert_str(&sanitized);
    }

    /// Applies one editing key, reporting whether the field consumed it. Callers
    /// keep the keys that mean something beyond editing (submit, cancel, list
    /// navigation) for themselves.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Left if control => self.move_word_left(),
            KeyCode::Right if control => self.move_word_right(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            KeyCode::Backspace => {
                self.backspace();
            }
            KeyCode::Delete => {
                self.delete();
            }
            KeyCode::Char('w') if control => {
                self.delete_word();
            }
            KeyCode::Char('u') if control => self.clear(),
            KeyCode::Char('a') if control => self.move_home(),
            KeyCode::Char('e') if control => self.move_end(),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_char(character);
            }
            _ => return false,
        }
        true
    }

    /// Puts the caret where a click at `column` of a one-row field `width`
    /// columns wide landed, the field showing the text the way
    /// [`field_window`] scrolled it for the caret as it was.
    pub fn click(&mut self, column: usize, width: u16) {
        let (start, _) = field_window(&self.text, self.cursor, width);
        self.cursor = char_at_column(&self.text, start, column);
    }

    fn character_count(&self) -> usize {
        self.text.chars().count()
    }

    /// The front of the whitespace-and-word run before the caret.
    fn word_start(&self) -> usize {
        let characters: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor.min(characters.len());
        while start > 0 && characters[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !characters[start - 1].is_whitespace() {
            start -= 1;
        }
        start
    }

    fn remove_range(&mut self, start: usize, end: usize) {
        let start_byte = byte_index(&self.text, start);
        let end_byte = byte_index(&self.text, end);
        self.text.replace_range(start_byte..end_byte, "");
    }
}

/// The terminal columns `text` paints in, measured the way the buffer
/// measures them: a CJK character is two, a combining mark none. Character
/// indices are not columns, which is why every caret goes through this.
#[must_use]
pub fn display_width(text: &str) -> usize {
    ratatui::text::Span::raw(text).width()
}

/// Where a one-row field `width` columns wide starts showing `text` so the
/// caret at character `cursor` is on it: the index of the first character
/// shown, and the caret's column within the field. The window moves by whole
/// characters, so a wide one is never cut at the left edge, and the caret has
/// a column of its own after the text before it — the end of the text
/// included.
#[must_use]
pub fn field_window(text: &str, cursor: usize, width: u16) -> (usize, u16) {
    let room = usize::from(width).max(1) - 1;
    let widths: Vec<usize> = text
        .chars()
        .map(|character| display_width(character.encode_utf8(&mut [0; 4])))
        .collect();
    let cursor = cursor.min(widths.len());
    let mut before: usize = widths[..cursor].iter().sum();
    let mut start = 0;
    while before > room && start < cursor {
        before -= widths[start];
        start += 1;
    }
    (start, u16::try_from(before).unwrap_or(u16::MAX))
}

/// The character a click at `column` lands on, in a field showing `text` from
/// character `start`: the one painted under that column, or the end of the
/// text past its last character. The caret goes in front of it.
#[must_use]
pub fn char_at_column(text: &str, start: usize, column: usize) -> usize {
    let mut x = 0;
    for (offset, character) in text.chars().skip(start).enumerate() {
        let width = display_width(character.encode_utf8(&mut [0; 4]));
        if x + width > column {
            return start + offset;
        }
        x += width;
    }
    text.chars().count()
}

fn byte_index(text: &str, character_index: usize) -> usize {
    text.char_indices()
        .nth(character_index)
        .map_or(text.len(), |(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn editing_keys_insert_and_delete_around_a_unicode_caret() {
        let mut input = TextInput::new("café");
        assert_eq!(input.cursor(), 4);

        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Char('x')));
        assert_eq!(input.text(), "cafxé");
        assert_eq!(input.cursor(), 4);

        assert!(input.handle_key(key(KeyCode::Backspace)));
        assert_eq!(input.text(), "café");
        assert_eq!(input.cursor(), 3);

        assert!(input.handle_key(key(KeyCode::Delete)));
        assert_eq!(input.text(), "caf");
        assert_eq!(input.cursor(), 3);

        input.handle_key(key(KeyCode::Home));
        assert_eq!(input.cursor(), 0);
        assert!(!input.backspace(), "nothing to delete at the start");
        assert!(!input.delete_word());
        input.handle_key(key(KeyCode::End));
        assert!(!input.delete(), "nothing to delete at the end");
        assert_eq!(input.text(), "caf");
    }

    #[test]
    fn word_deletion_takes_trailing_space_and_the_word_before_it() {
        let mut input = TextInput::new("alpha café");
        assert!(input.handle_key(control(KeyCode::Char('w'))));
        assert_eq!(input.text(), "alpha ");
        assert_eq!(input.cursor(), 6);

        assert!(input.handle_key(control(KeyCode::Char('w'))));
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);

        let mut clearing = TextInput::new("alpha beta");
        clearing.set_cursor(5);
        assert!(clearing.handle_key(control(KeyCode::Char('u'))));
        assert!(clearing.is_empty());
        assert_eq!(clearing.cursor(), 0);
    }

    #[test]
    fn word_motion_steps_over_a_word_and_the_space_beside_it() {
        let mut input = TextInput::new("alpha  café");
        assert!(input.handle_key(control(KeyCode::Left)));
        assert_eq!(input.cursor(), 7, "the front of the word under the caret");
        assert!(input.handle_key(control(KeyCode::Left)));
        assert_eq!(input.cursor(), 0, "the space and the word before it");
        input.handle_key(control(KeyCode::Left));
        assert_eq!(input.cursor(), 0, "the start is as far left as it goes");

        assert!(input.handle_key(control(KeyCode::Right)));
        assert_eq!(input.cursor(), 5);
        assert!(input.handle_key(control(KeyCode::Right)));
        assert_eq!(input.cursor(), 11);
        input.handle_key(control(KeyCode::Right));
        assert_eq!(input.cursor(), 11, "the end is as far right as it goes");
        assert_eq!(input.text(), "alpha  café", "motion edits nothing");
    }

    #[test]
    fn the_line_keys_jump_to_either_end() {
        let mut input = TextInput::new("alpha");
        assert!(input.handle_key(control(KeyCode::Char('a'))));
        assert_eq!(input.cursor(), 0);
        assert!(input.handle_key(control(KeyCode::Char('e'))));
        assert_eq!(input.cursor(), 5);
        assert_eq!(input.text(), "alpha", "neither key types its letter");
    }

    #[test]
    fn paste_folds_line_breaks_into_spaces_and_strips_the_rest() {
        let mut query = TextInput::new("alpha ");
        query.paste("tea\nshop\u{7}");
        assert_eq!(query.text(), "alpha tea shop");
        assert_eq!(query.cursor(), 14);

        let mut middle = TextInput::new("ab");
        middle.set_cursor(1);
        middle.paste("\u{7}");
        assert_eq!(middle.text(), "ab", "an all-control paste inserts nothing");
        assert_eq!(middle.cursor(), 1);
    }

    #[test]
    fn cursor_is_clamped_and_non_editing_keys_are_left_alone() {
        let mut input = TextInput::new("abc");
        input.set_cursor(99);
        assert_eq!(input.cursor(), 3);
        input.move_right();
        assert_eq!(input.cursor(), 3);

        input.set_text("é");
        assert_eq!(input.cursor(), 1);
        input.set_cursor(0);
        input.move_left();
        assert_eq!(input.cursor(), 0);

        assert!(!input.handle_key(key(KeyCode::Enter)));
        assert!(!input.handle_key(key(KeyCode::Up)));
        assert!(!input.handle_key(control(KeyCode::Char('p'))));
        assert!(
            !input.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)),
            "alt chords belong to the caller"
        );
        assert_eq!(input.text(), "é");
    }

    #[test]
    fn a_field_scrolls_by_whole_characters_to_keep_the_caret_on_it() {
        // Everything fits: nothing scrolls and the caret is where the text is.
        assert_eq!(field_window("abc", 3, 10), (0, 3));
        assert_eq!(field_window("abc", 0, 10), (0, 0));
        // Ten characters in a field of six: the caret at the end needs the
        // last five in front of it.
        assert_eq!(field_window("abcdefghij", 10, 6), (5, 5));
        assert_eq!(field_window("abcdefghij", 5, 6), (0, 5));
        assert_eq!(field_window("abcdefghij", 6, 6), (1, 5));
        // A wide character is two columns, so the caret after 日本 is at 4,
        // and a window that cannot hold 日 whole moves past it entirely.
        assert_eq!(field_window("日本語", 2, 10), (0, 4));
        assert_eq!(field_window("日本語", 3, 4), (2, 2));
        // A combining mark takes no column of its own.
        assert_eq!(field_window("e\u{301}x", 2, 10), (0, 1));
        assert_eq!(field_window("e\u{301}x", 3, 10), (0, 2));
        assert_eq!(
            field_window("abc", 9, 10),
            (0, 3),
            "a caret past the end sits at the end"
        );
        assert_eq!(field_window("abc", 3, 0), (3, 0), "no width is one column");
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("e\u{301}"), 1);
    }

    #[test]
    fn a_click_lands_on_the_character_under_its_column() {
        assert_eq!(char_at_column("abc", 0, 1), 1);
        assert_eq!(char_at_column("abc", 0, 7), 3, "past the end is the end");
        assert_eq!(char_at_column("abcdefghij", 5, 2), 7, "in a scrolled field");
        assert_eq!(
            char_at_column("日本語", 0, 1),
            0,
            "either column of a wide character is that character"
        );
        assert_eq!(char_at_column("日本語", 0, 2), 1);
        assert_eq!(
            char_at_column("e\u{301}x", 0, 1),
            2,
            "a mark and its base are one column"
        );

        let mut input = TextInput::new("abcdefghij");
        input.click(2, 6);
        assert_eq!(
            input.cursor(),
            7,
            "the field was scrolled for the caret at the end"
        );
        input.move_home();
        input.click(2, 6);
        assert_eq!(input.cursor(), 2, "and not once the caret is at the start");
    }
}
