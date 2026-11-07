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

/// The face for `family` — its regular weight and upright style, or as near
/// as the family has — or the default face for `None` or a family that is not
/// installed or will not load.
pub fn face(family: Option<&str>) -> Arc<Font> {
    let Some(family) = family else {
        return default_face();
    };
    static LOADED: OnceLock<Mutex<HashMap<String, Arc<Font>>>> = OnceLock::new();
    let loaded = LOADED.get_or_init(Default::default);
    if let Some(font) = loaded.lock().unwrap().get(family) {
        return font.clone();
    }
    let db = system();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..fontdb::Query::default()
    };
    let font = db
        .query(&query)
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
    loaded.lock().unwrap().insert(family.to_string(), font.clone());
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_family_falls_back_rather_than_failing() {
        let font = face(Some("No Such Family 4f2a"));
        assert!(font.lookup_glyph_index('A') != 0);
    }

    #[test]
    fn the_default_face_draws_letters() {
        assert!(face(None).lookup_glyph_index('A') != 0);
    }
}
