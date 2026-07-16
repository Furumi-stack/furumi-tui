//! One-line text editing with a movable cursor, shared by every text input
//! (command line, edit forms, popup fields).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single-line value plus a cursor position (in characters). Dereferences
/// to `&str`, so read paths treat it like the plain string it wraps.
#[derive(Debug, Clone, Default)]
pub struct LineEdit {
    value: String,
    cursor: usize,
}

impl LineEdit {
    /// Starts with `value` and the cursor at its end.
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Cursor position in characters (0..=len).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.value
            .char_indices()
            .nth(chars)
            .map(|(index, _)| index)
            .unwrap_or(self.value.len())
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_index(self.cursor);
        self.value.insert_str(at, s);
        self.cursor += s.chars().count();
    }

    /// Removes the character before the cursor; `false` when at the start.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let at = self.byte_index(self.cursor - 1);
        self.value.remove(at);
        self.cursor -= 1;
        true
    }

    /// Removes the character under the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.value.chars().count() {
            let at = self.byte_index(self.cursor);
            self.value.remove(at);
        }
    }

    /// Applies one editing key (characters, backspace/delete, cursor
    /// movement). Returns `false` for keys this editor does not handle
    /// (Enter, Esc, Tab, ...), which the caller interprets itself.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                self.insert(c);
            }
            KeyCode::Backspace => {
                self.backspace();
            }
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            _ => return false,
        }
        true
    }
}

impl std::ops::Deref for LineEdit {
    type Target = str;

    fn deref(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Display for LineEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn edits_at_cursor() {
        let mut edit = LineEdit::new("hllo");
        assert_eq!(edit.cursor(), 4);
        edit.handle_key(key(KeyCode::Home));
        edit.handle_key(key(KeyCode::Right));
        edit.handle_key(key(KeyCode::Char('e')));
        assert_eq!(edit.as_str(), "hello");
        assert_eq!(edit.cursor(), 2);
        edit.handle_key(key(KeyCode::End));
        edit.handle_key(key(KeyCode::Backspace));
        assert_eq!(edit.as_str(), "hell");
        edit.handle_key(key(KeyCode::Home));
        edit.handle_key(key(KeyCode::Delete));
        assert_eq!(edit.as_str(), "ell");
    }

    #[test]
    fn multibyte_safe() {
        let mut edit = LineEdit::new("метл");
        edit.handle_key(key(KeyCode::Left));
        edit.insert('а');
        assert_eq!(edit.as_str(), "метал");
        edit.handle_key(key(KeyCode::End));
        edit.insert('л');
        assert_eq!(edit.as_str(), "металл");
    }
}
