//! Colours as the picker deals in them: hue, saturation and value; hex codes;
//! and the colours picked lately, kept between sessions.
//!
//! Everything is in the UI's own convention — straight RGBA, sRGB-encoded,
//! 0 to 1 — so a colour read off the screen, typed as hex or picked from the
//! square means the same thing a title stores.

/// How many recent colours are kept.
pub const RECENT_MAX: usize = 8;

/// `[r, g, b]` as `[hue, saturation, value]`, each 0 to 1. A grey has no hue
/// of its own and reports 0; the picker keeps its own hue across greys for
/// that reason, rather than reading it back from the colour.
pub fn rgb_to_hsv([r, g, b]: [f32; 3]) -> [f32; 3] {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    let s = if max <= 0.0 { 0.0 } else { d / max };
    [h, s, max]
}

pub fn hsv_to_rgb([h, s, v]: [f32; 3]) -> [f32; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

/// `RRGGBB`, upper case, without the `#`.
pub fn to_hex(c: [f32; 4]) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{:02X}{:02X}{:02X}", byte(c[0]), byte(c[1]), byte(c[2]))
}

/// A colour from `RRGGBB` or `RGB`, with or without a leading `#`, opaque.
pub fn from_hex(text: &str) -> Option<[f32; 4]> {
    let t = text.trim().trim_start_matches('#');
    if !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full: String = match t.len() {
        3 => t.chars().flat_map(|c| [c, c]).collect(),
        6 => t.to_string(),
        _ => return None,
    };
    let at = |i: usize| u8::from_str_radix(&full[i..i + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([at(0)?, at(2)?, at(4)?, 1.0])
}

/// Whether two colours would look the same in an 8-bit picture.
pub fn same(a: [f32; 4], b: [f32; 4]) -> bool {
    (0..3).all(|i| (a[i] - b[i]).abs() < 0.5 / 255.0)
}

/// `color` at the front of `recent`, without repeats, at most [`RECENT_MAX`].
pub fn remember(recent: &mut Vec<[f32; 4]>, color: [f32; 4]) {
    recent.retain(|c| !same(*c, color));
    recent.insert(0, color);
    recent.truncate(RECENT_MAX);
}

/// Where the recent colours are kept: in the user's own config, not the
/// project, so an end card's colours are at hand in the next short too.
const RECENT_FILE: &str = "recent-colours";

/// The recent colours saved last time, or none. A file that will not read is
/// the same as no file: losing the list is not worth an error.
pub fn load_recent() -> Vec<[f32; 4]> {
    let Some(text) = crate::config::read(RECENT_FILE) else {
        return Vec::new();
    };
    text.lines().filter_map(from_hex).take(RECENT_MAX).collect()
}

pub fn save_recent(recent: &[[f32; 4]]) {
    let text: String = recent.iter().map(|c| to_hex(*c) + "\n").collect();
    crate::config::write(RECENT_FILE, &text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_round_trips_through_rgb() {
        for rgb in [[1.0, 0.0, 0.0], [0.2, 0.6, 0.9], [1.0, 0.55, 0.12], [0.3, 0.3, 0.3], [0.0, 0.0, 0.0]] {
            let back = hsv_to_rgb(rgb_to_hsv(rgb));
            for i in 0..3 {
                assert!((back[i] - rgb[i]).abs() < 1e-5, "{rgb:?} came back {back:?}");
            }
        }
        assert_eq!(hsv_to_rgb([1.0 / 3.0, 1.0, 1.0]), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn hex_reads_both_lengths_and_writes_six() {
        assert_eq!(from_hex("#FF8000"), Some([1.0, 128.0 / 255.0, 0.0, 1.0]));
        assert_eq!(from_hex("f80"), from_hex("FF8800"));
        assert_eq!(from_hex("12345"), None);
        assert_eq!(from_hex("GG0000"), None);
        assert_eq!(to_hex([1.0, 128.0 / 255.0, 0.0, 1.0]), "FF8000");
    }

    #[test]
    fn a_remembered_colour_moves_to_the_front_once() {
        let mut recent = vec![[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0]];
        remember(&mut recent, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(recent, vec![[0.0, 1.0, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0]]);
        for i in 0..20 {
            remember(&mut recent, [i as f32 / 20.0, 0.0, 0.0, 1.0]);
        }
        assert_eq!(recent.len(), RECENT_MAX);
    }
}
