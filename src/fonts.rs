//! The faces a title can be set in: the UI's own, which always exists, and
//! whatever is installed on the system.
//!
//! Shared by the render thread and the export worker, so everything here sits
//! behind process-wide caches. The system scan is slow enough to notice, so it
//! runs the first time anything asks for a family by name and never again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use fontdue::{Font, FontSettings};

use crate::text::FONT_BYTES;

fn system() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

/// The UI's own face, which a title uses until another is picked — and falls
/// back to when the picked one is not installed, so a project moved to another
/// machine still shows its words.
fn default_face() -> Arc<Font> {
    static FONT: OnceLock<Arc<Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        Arc::new(
            Font::from_bytes(FONT_BYTES, FontSettings::default())
                .expect("failed to parse the bundled font"),
        )
    })
    .clone()
}

/// Every installed family, by name, sorted and without repeats.
pub fn families() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| {
        let mut names: Vec<String> = system()
            .faces()
            .filter_map(|f| f.families.first().map(|(name, _)| name.clone()))
            .collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        names
    })
}

/// How many entries the font list has: the default, then every family.
pub fn choice_count() -> usize {
    families().len() + 1
}

/// The family entry `i` of the font list picks: `None` for the first, which
/// is the default face.
pub fn choice(i: usize) -> Option<&'static str> {
    i.checked_sub(1).and_then(|i| families().get(i)).map(String::as_str)
}

/// The weight a title starts at, and the only one the default face has.
pub const REGULAR: u16 = 400;

/// The face `family` picks for `weight`: the installed face nearest it, by the
/// same rule CSS uses, upright. `None` when the family is not installed.
fn resolve(family: &str, weight: u16) -> Option<fontdb::ID> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight(weight),
        ..fontdb::Query::default()
    };
    system().query(&query)
}

/// The weights `family` comes in, upright, lightest first. The default face
/// has the one.
pub fn weights(family: Option<&str>) -> Vec<u16> {
    let Some(family) = family else {
        return vec![REGULAR];
    };
    let mut found: Vec<u16> = system()
        .faces()
        .filter(|f| f.style == fontdb::Style::Normal)
        .filter(|f| f.families.iter().any(|(name, _)| name == family))
        .map(|f| f.weight.0)
        .collect();
    found.sort_unstable();
    found.dedup();
    if found.is_empty() {
        found.push(REGULAR);
    }
    found
}

/// The weight `family` will actually draw for `weight`: the nearest it has.
pub fn shown_weight(family: Option<&str>, weight: u16) -> u16 {
    family
        .and_then(|f| resolve(f, weight))
        .and_then(|id| system().face(id))
        .map_or(REGULAR, |f| f.weight.0)
}

/// A weight by the name type designers give it.
pub fn weight_name(weight: u16) -> String {
    let name = match weight {
        100 => "Thin",
        200 => "ExtraLight",
        300 => "Light",
        400 => "Regular",
        500 => "Medium",
        600 => "SemiBold",
        700 => "Bold",
        800 => "ExtraBold",
        900 => "Black",
        other => return other.to_string(),
    };
    name.to_string()
}

/// The face for `family` at `weight` — or as near as the family has, upright
/// — or the default face for `None` or a family that is not installed or will
/// not load.
pub fn face(family: Option<&str>, weight: u16) -> Arc<Font> {
    let Some(family) = family else {
        return default_face();
    };
    static LOADED: OnceLock<Mutex<HashMap<(String, u16), Arc<Font>>>> = OnceLock::new();
    let loaded = LOADED.get_or_init(Default::default);
    let key = (family.to_string(), weight);
    if let Some(font) = loaded.lock().unwrap().get(&key) {
        return font.clone();
    }
    let db = system();
    let font = resolve(family, weight)
        .and_then(|id| {
            db.with_face_data(id, |data, index| {
                let settings = FontSettings { collection_index: index, ..FontSettings::default() };
                Font::from_bytes(data, settings).ok()
            })
        })
        .flatten()
        .map(Arc::new);
    let font = match font {
        Some(font) => font,
        None => {
            log::warn!("font {family:?} is not available; using the default");
            default_face()
        }
    };
    loaded.lock().unwrap().insert(key, font.clone());
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_family_falls_back_rather_than_failing() {
        let font = face(Some("No Such Family 4f2a"), REGULAR);
        assert!(font.lookup_glyph_index('A') != 0);
    }

    #[test]
    fn the_default_face_draws_letters() {
        assert!(face(None, REGULAR).lookup_glyph_index('A') != 0);
    }

    #[test]
    fn weights_are_named_and_the_default_face_has_one() {
        assert_eq!(weights(None), vec![REGULAR]);
        assert_eq!(weights(Some("No Such Family 4f2a")), vec![REGULAR]);
        assert_eq!(weight_name(600), "SemiBold");
        assert_eq!(weight_name(450), "450");
    }
}
