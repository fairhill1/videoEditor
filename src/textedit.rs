//! A caret and a selection in a line or a block of text, and what typing,
//! deleting and the clipboard do to them. Shared by every box that takes
//! typing — a title's text, the colour picker's hex code — so each behaves
//! as text boxes do everywhere else.
//!
//! Positions are byte offsets on character boundaries, which is what
//! `String`'s own editing takes.

/// Where the caret is, and where the selection runs from to it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub caret: usize,
    /// The other end of the selection. `None`, or the caret itself, is no
    /// selection.
    pub anchor: Option<usize>,
}

impl Cursor {
    pub fn at(i: usize) -> Self {
        Self { caret: i, anchor: None }
    }

    /// Everything in `text` selected, the caret at the end.
    pub fn all(text: &str) -> Self {
        Self { caret: text.len(), anchor: Some(0) }
    }

    /// The selection as a range, when there is one.
    pub fn range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    pub fn selected<'a>(&self, text: &'a str) -> &'a str {
        self.range().map_or("", |(a, b)| &text[a..b])
    }

    /// Held inside `text` and on its character boundaries: an undo or
    /// another edit can have shortened it under the caret.
    pub fn clamp(&mut self, text: &str) {
        let fix = |i: usize| {
            let mut i = i.min(text.len());
            while !text.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        self.caret = fix(self.caret);
        self.anchor = self.anchor.map(fix);
    }

    /// Put the caret at `i`, taking the selection with it when `extend`
    /// (Shift held, or a drag), and dropping it otherwise.
    pub fn move_to(&mut self, i: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = i;
    }

    /// Move the caret by `step`. Without `extend`, a selection collapses to
    /// its end on the side the step goes instead, as the arrows do in any
    /// text box.
    pub fn step(&mut self, text: &str, step: impl Fn(&str, usize) -> usize, extend: bool) {
        let to = step(text, self.caret);
        match self.range() {
            Some((a, b)) if !extend => {
                self.anchor = None;
                self.caret = if to < self.caret { a } else if to > self.caret { b } else { to };
            }
            _ => self.move_to(to, extend),
        }
    }

    /// Type `s` at the caret, over the selection if there is one.
    pub fn insert(&mut self, text: &mut String, s: &str) {
        let (a, b) = self.range().unwrap_or((self.caret, self.caret));
        text.replace_range(a..b, s);
        *self = Self::at(a + s.len());
    }

    /// The selection, or the character before the caret.
    pub fn backspace(&mut self, text: &mut String) {
        if self.range().is_some() {
            return self.insert(text, "");
        }
        let from = text[..self.caret].char_indices().next_back().map_or(0, |(j, _)| j);
        text.replace_range(from..self.caret, "");
        *self = Self::at(from);
    }

    /// The selection, or the character after the caret.
    pub fn delete(&mut self, text: &mut String) {
        if self.range().is_some() {
            return self.insert(text, "");
        }
        let to = text[self.caret..].chars().next().map_or(self.caret, |c| self.caret + c.len_utf8());
        text.replace_range(self.caret..to, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn left(t: &str, i: usize) -> usize {
        t[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    fn right(t: &str, i: usize) -> usize {
        t[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    #[test]
    fn shift_steps_select_and_typing_replaces_the_selection() {
        let mut text = String::from("héllo");
        let mut c = Cursor::at(0);
        c.step(&text, right, true);
        c.step(&text, right, true);
        assert_eq!(c.selected(&text), "hé");
        c.insert(&mut text, "J");
        assert_eq!(text, "Jllo");
        assert_eq!(c, Cursor::at(1));
    }

    #[test]
    fn an_arrow_without_shift_collapses_to_the_side_it_goes() {
        let text = "abcdef";
        let mut c = Cursor { caret: 4, anchor: Some(1) };
        c.step(text, left, false);
        assert_eq!(c, Cursor::at(1));
        let mut c = Cursor { caret: 1, anchor: Some(4) };
        c.step(text, right, false);
        assert_eq!(c, Cursor::at(4));
    }

    #[test]
    fn deleting_takes_the_selection_or_one_character() {
        let mut text = String::from("abc");
        let mut c = Cursor::all(&text);
        c.backspace(&mut text);
        assert_eq!((text.as_str(), c), ("", Cursor::at(0)));
        let mut text = String::from("aé");
        let mut c = Cursor::at(text.len());
        c.backspace(&mut text);
        assert_eq!(text, "a");
        let mut c2 = Cursor::at(0);
        c2.delete(&mut text);
        assert_eq!(text, "");
        c.clamp(&text);
        assert_eq!(c.caret, 0);
    }
}
