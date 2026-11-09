//! Saved title styles: a title's look kept under a name, so the next caption
//! is one pick rather than a dozen settings made again.
//!
//! Kept in the user's config rather than the project, so the styles a channel
//! settles on are there in every project. Picking one restyles every title
//! the effects tab is styling, the way the panel's own controls do.
//!
//! A look only, not a place: captions sit by whoever is speaking, so a style
//! that moved its clips would undo that placement every time it was picked.
//! Where a clip sits stays the clip's own, as it is for every other setting.

use serde::{Deserialize, Serialize};

use crate::layout::TitleMenu;
use crate::state::State;
use crate::textedit::Cursor;
use crate::title::Title;

const FILE: &str = "title-styles.ron";

/// What a style is named until it is given a name.
const DEFAULT_NAME: &str = "Style";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Style {
    pub name: String,
    /// Everything about a title but its words, which stay the title's own.
    /// A whole title rather than a list of its fields, so a setting added to
    /// titles later is in every style saved after it without a change here.
    pub look: Title,
}

impl Style {
    pub fn new(name: String, title: &Title) -> Self {
        Self { name, look: Title { text: String::new(), ..title.clone() } }
    }

    /// `title` restyled: this style's look around the title's own words.
    pub fn restyle(&self, title: &Title) -> Title {
        Title { text: title.text.clone(), ..self.look.clone() }
    }

    /// Whether `title` is in this style, whatever it says.
    pub fn matches(&self, title: &Title) -> bool {
        self.look == Title { text: String::new(), ..title.clone() }
    }
}

/// The styles saved last time, or none. A file that will not read is logged
/// and left alone until the next save.
pub fn load() -> Vec<Style> {
    let Some(text) = crate::config::read(FILE) else {
        return Vec::new();
    };
    ron::from_str(&text).unwrap_or_else(|e| {
        log::warn!("could not read the saved title styles: {e}");
        Vec::new()
    })
}

pub fn save(styles: &[Style]) {
    match ron::ser::to_string_pretty(styles, ron::ser::PrettyConfig::default()) {
        Ok(text) => crate::config::write(FILE, &text),
        Err(e) => log::warn!("could not write the title styles: {e}"),
    }
}

/// `Style 1`, `Style 2`, ...: the first not already taken.
pub fn unused_name(styles: &[Style]) -> String {
    (1..)
        .map(|n| format!("{DEFAULT_NAME} {n}"))
        .find(|name| !styles.iter().any(|s| &s.name == name))
        .expect("an unused name")
}

/// A typed name as a style keeps it: on one line, without surrounding space.
fn clean_name(typed: &str) -> String {
    typed.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// The style name being typed: which style, and the caret in its name.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Naming {
    pub(crate) index: usize,
    pub(crate) cursor: Cursor,
}

impl State {
    /// Which saved style the title the effects tab shows is in, if any.
    pub(crate) fn shown_style(&self) -> Option<usize> {
        let (_, title) = self.effects_title()?;
        self.title_styles.iter().position(|s| s.matches(title))
    }

    /// Save the shown title's look as a new style, and start typing its name.
    pub(crate) fn save_shown_style(&mut self) {
        let Some((_, title)) = self.effects_title() else {
            return;
        };
        let style = Style::new(unused_name(&self.title_styles), title);
        self.font_menu = None;
        self.title_styles.push(style);
        save(&self.title_styles);
        let index = self.title_styles.len() - 1;
        let cursor = Cursor::all(&self.title_styles[index].name);
        self.naming_style = Some(Naming { index, cursor });
    }

    /// Restyle every title the effects tab is styling in saved style `index`,
    /// as one undo step. Each keeps its words and its clip stays where it is.
    pub(crate) fn apply_style(&mut self, index: usize) {
        let Some(style) = self.title_styles.get(index).cloned() else {
            return;
        };
        self.change_shown_title(|t| *t = style.restyle(t));
    }

    /// Forget saved style `index`. Titles already in it keep their look.
    pub(crate) fn delete_style(&mut self, index: usize) {
        if index >= self.title_styles.len() {
            return;
        }
        self.finish_style_name();
        self.title_styles.remove(index);
        save(&self.title_styles);
    }

    /// Drop the list of saved styles from the style box.
    pub(crate) fn open_style_menu(&mut self) {
        if self.effects_title().is_some() {
            self.font_menu = Some((TitleMenu::Style, 0));
        }
    }

    pub(crate) fn typing_style_name(&self) -> bool {
        self.naming_style.is_some()
    }

    /// The name being typed and its caret, to change.
    fn edit_style_name(&mut self, change: impl FnOnce(&mut String, &mut Cursor)) {
        let Some(naming) = self.naming_style.as_mut() else {
            return;
        };
        let Some(style) = self.title_styles.get_mut(naming.index) else {
            self.naming_style = None;
            return;
        };
        naming.cursor.clamp(&style.name);
        change(&mut style.name, &mut naming.cursor);
        naming.cursor.clamp(&style.name);
    }

    pub(crate) fn type_style_name(&mut self, typed: &str) {
        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
        if !typed.is_empty() {
            self.edit_style_name(|name, cursor| cursor.insert(name, &typed));
        }
    }

    pub(crate) fn backspace_style_name(&mut self) {
        self.edit_style_name(|name, cursor| cursor.backspace(name));
    }

    pub(crate) fn delete_in_style_name(&mut self) {
        self.edit_style_name(|name, cursor| cursor.delete(name));
    }

    pub(crate) fn move_style_name_caret(&mut self, step: fn(&str, usize) -> usize, extend: bool) {
        self.edit_style_name(|name, cursor| cursor.step(name, step, extend));
    }

    pub(crate) fn select_all_in_style_name(&mut self) {
        self.edit_style_name(|name, cursor| *cursor = Cursor::all(name));
    }

    /// Copy the selected part of the name; with `cut`, take it out too.
    pub(crate) fn copy_from_style_name(&mut self, cut: bool) {
        self.edit_style_name(|name, cursor| {
            let picked = cursor.selected(name).to_string();
            if picked.is_empty() {
                return;
            }
            crate::clipboard::copy(&picked);
            if cut {
                cursor.insert(name, "");
            }
        });
    }

    pub(crate) fn paste_into_style_name(&mut self) {
        let pasted = crate::clipboard::paste();
        self.type_style_name(&pasted);
    }

    /// Stop typing the name and keep it. A name left blank goes back to a
    /// numbered one rather than leaving a row with nothing in it.
    pub(crate) fn finish_style_name(&mut self) {
        let Some(naming) = self.naming_style.take() else {
            return;
        };
        let Some(typed) = self.title_styles.get(naming.index).map(|s| clean_name(&s.name)) else {
            return;
        };
        let name = if typed.is_empty() {
            let others: Vec<Style> = self
                .title_styles
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != naming.index)
                .map(|(_, s)| s.clone())
                .collect();
            unused_name(&others)
        } else {
            typed
        };
        self.title_styles[naming.index].name = name;
        save(&self.title_styles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::title::{Background, Outline, Shadow};

    fn styled() -> Title {
        Title {
            text: "Hello".into(),
            size: 0.07,
            color: [1.0, 0.84, 0.12, 1.0],
            font: Some("Inter".into()),
            weight: 700,
            outline: Outline { enabled: true, width: 0.005, color: [0.0, 0.0, 0.0, 1.0] },
            shadow: Shadow { enabled: true, ..Shadow::default() },
            background: Background { enabled: true, ..Background::default() },
            line_spacing: 1.2,
        }
    }

    #[test]
    fn a_style_carries_the_look_but_not_the_words() {
        let style = Style::new("Caption".into(), &styled());
        assert!(style.look.text.is_empty());
        let plain = Title { text: "Other words".into(), ..Title::default() };
        let restyled = style.restyle(&plain);
        assert_eq!(restyled.text, "Other words");
        assert_eq!(Title { text: "Hello".into(), ..restyled.clone() }, styled());
        assert!(style.matches(&restyled));
    }

    #[test]
    fn a_title_changed_is_out_of_its_style() {
        let style = Style::new("Caption".into(), &styled());
        assert!(style.matches(&Title { text: "Anything".into(), ..styled() }));
        assert!(!style.matches(&Title { size: 0.08, ..styled() }));
    }

    /// Styles saved when they still held a position read as their look.
    #[test]
    fn an_old_style_with_a_placement_still_reads() {
        let old = r#"[(name: "Caption", look: (text: "", size: 0.07, color: (1.0, 0.0, 0.0, 1.0)),
            placement: (x: 0.0, y: -0.17, scale: 1.0))]"#;
        let styles: Vec<Style> = ron::from_str(old).unwrap();
        assert_eq!(styles[0].look.size, 0.07);
    }

    #[test]
    fn new_styles_take_the_first_free_number() {
        let named = |n: &str| Style::new(n.into(), &Title::default());
        assert_eq!(unused_name(&[]), "Style 1");
        assert_eq!(unused_name(&[named("Style 1"), named("Style 3")]), "Style 2");
    }

    #[test]
    fn styles_round_trip_through_their_file_format() {
        let styles = vec![Style::new("Caption".into(), &styled())];
        let text = ron::ser::to_string_pretty(&styles, ron::ser::PrettyConfig::default()).unwrap();
        let back: Vec<Style> = ron::from_str(&text).unwrap();
        assert_eq!(back, styles);
    }

    #[test]
    fn a_typed_name_is_one_trimmed_line() {
        assert_eq!(clean_name("  Lower third\t"), "Lower third");
        assert_eq!(clean_name("   "), "");
    }
}
