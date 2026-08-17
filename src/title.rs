//! Titles: text as a source of pictures.
//!
//! A title is a pool source like any other. It has no file behind it, so
//! instead of a decoder it has this: a rasterizer that turns its text into a
//! coverage mask the size of the canvas. From there it is an ordinary layer —
//! trimmed, faded, placed and stacked by the same code that handles footage,
//! rather than a special case the compositor has to know about.
//!
//! A mask rather than a picture because a title is one colour. What varies
//! across the frame is only how much of that colour is present, which is
//! exactly what a glyph rasterizer produces; carrying three more channels of
//! the same number would cost four times the memory to say the same thing.
//!
//! Sized to the canvas so that placement means the same for a title as for a
//! clip: the neutral transform fills the frame, and everything about moving and
//! scaling one is the arithmetic already written for the other.

use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use serde::{Deserialize, Serialize};

use crate::text::FONT_BYTES;

/// Cap height of a new title, as a fraction of the canvas height. Large enough
/// to read as a title rather than a caption, and small enough that a few words
/// fit across the frame.
pub const DEFAULT_SIZE: f32 = 0.12;

/// How long a title lands on the timeline when dropped, in seconds. A title has
/// no intrinsic length — unlike a file, nothing about it says when it ends — so
/// this is a starting point to trim from rather than a limit.
pub const DEFAULT_DURATION: f64 = 5.0;

/// The longest a title clip can be trimmed to. Nothing runs out, so this only
/// exists because the trim needs a number; an hour is past any title anyone
/// means to hold on screen.
pub const MAX_DURATION: f64 = 3600.0;

/// Fraction of the line height left between lines, on top of the font's own
/// ascent and descent.
const LINE_GAP: f32 = 0.15;

/// Stands in for the text cursor while a title is being typed into. A full
/// block rather than a hairline: it is drawn at title size on a picture, where
/// a one-pixel bar would be invisible on some frames and a stray mark on
/// others.
pub const CARET: char = '\u{2588}';

/// The text `Title` starts out as. Never empty: a title with nothing in it
/// rasterizes to nothing, and a pool row that shows nothing and puts nothing on
/// the canvas reads as a bug rather than as an empty title.
pub const PLACEHOLDER: &str = "Title";

/// A generated picture: some words, at a size, in a colour.
///
/// Where it sits on the canvas is deliberately *not* here — that is the clip's
/// transform, the same one footage uses. A title pinned to the lower third is
/// a placement, and placement belongs to the clip rather than to the source, so
/// that two clips of one title can sit in two places.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Title {
    pub text: String,
    /// Cap height as a fraction of the canvas height, so a title holds its
    /// proportions on any canvas.
    pub size: f32,
    /// Straight (non-premultiplied) RGBA, in the same sRGB convention the rest
    /// of the UI's colours use.
    pub color: [f32; 4],
}

impl Default for Title {
    fn default() -> Self {
        Self {
            text: PLACEHOLDER.to_string(),
            size: DEFAULT_SIZE,
            color: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

impl Title {
    /// The name this title shows in the media pool: its first line, which is
    /// what distinguishes one title from another at a glance.
    pub fn pool_name(&self) -> &str {
        let first = self.text.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            PLACEHOLDER
        } else {
            first
        }
    }
}

/// How much of the title's colour reaches each pixel of the canvas, 0 to 255.
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// The face titles are set in — the UI's own, loaded once and shared.
///
/// Separate from [`crate::text::TextRenderer`]'s copy because the two want
/// different things from it: that one rasterizes into a GPU atlas at screen
/// sizes on the render thread, this one rasterizes whole words at canvas sizes
/// and has to work on the export worker too.
fn font() -> &'static Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| {
        Font::from_bytes(FONT_BYTES, FontSettings::default()).expect("failed to parse font")
    })
}

/// Draw `title` into a `width x height` coverage mask.
///
/// Lines are centred against each other and the block is centred in the frame,
/// which is what a title with no further instruction means. Anything else —
/// left-aligned, pinned to a corner — is the clip's transform moving the whole
/// block, so there is one way to place a title rather than two that interact.
pub fn rasterize(title: &Title, width: u32, height: u32) -> Mask {
    let mut mask = Mask {
        width,
        height,
        coverage: vec![0; (width as usize) * (height as usize)],
    };
    if width == 0 || height == 0 {
        return mask;
    }
    let px = (title.size * height as f32).max(1.0);
    let font = font();
    let metrics = font
        .horizontal_line_metrics(px)
        .expect("font has no horizontal line metrics");
    let line_h = (metrics.ascent - metrics.descent) * (1.0 + LINE_GAP);

    let lines: Vec<&str> = title.text.lines().collect();
    let block_h = line_h * lines.len() as f32;
    // The first baseline: the block centred, then down by one ascent to get
    // from the top of the first line to the line it sits on.
    let mut baseline = (height as f32 - block_h) * 0.5 + metrics.ascent;

    for line in lines {
        let line_w: f32 = line
            .chars()
            .map(|ch| font.metrics(ch, px).advance_width)
            .sum();
        let mut pen = (width as f32 - line_w) * 0.5;
        for ch in line.chars() {
            let (m, bitmap) = font.rasterize(ch, px);
            if m.width > 0 && m.height > 0 {
                let gx = (pen + m.xmin as f32).round() as i32;
                let gy = (baseline - (m.ymin + m.height as i32) as f32).round() as i32;
                stamp(&mut mask, &bitmap, m.width as u32, m.height as u32, gx, gy);
            }
            pen += m.advance_width;
        }
        baseline += line_h;
    }
    mask
}

/// Lay a glyph's coverage into the mask at `(x, y)`, keeping whichever of the
/// two is stronger.
///
/// `max` rather than a sum: glyph boxes overlap where letters tuck under each
/// other, and adding coverage there would build a bright seam along the join
/// that is not part of either letter.
fn stamp(mask: &mut Mask, bitmap: &[u8], w: u32, h: u32, x: i32, y: i32) {
    for row in 0..h as i32 {
        let dy = y + row;
        if dy < 0 || dy >= mask.height as i32 {
            continue;
        }
        for col in 0..w as i32 {
            let dx = x + col;
            if dx < 0 || dx >= mask.width as i32 {
                continue;
            }
            let src = bitmap[(row as u32 * w + col as u32) as usize];
            let dst = &mut mask.coverage[(dy as u32 * mask.width + dx as u32) as usize];
            *dst = (*dst).max(src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(mask: &Mask, x: u32, y: u32) -> u8 {
        mask.coverage[(y * mask.width + x) as usize]
    }

    fn ink(mask: &Mask) -> usize {
        mask.coverage.iter().filter(|&&c| c > 0).count()
    }

    #[test]
    fn a_title_puts_ink_on_the_canvas() {
        let mask = rasterize(&Title::default(), 640, 360);
        assert_eq!((mask.width, mask.height), (640, 360));
        assert!(ink(&mask) > 0, "the default title rasterized to nothing");
    }

    /// Nothing in the text is nothing on the canvas — not a block, not a
    /// stripe. A title being cleared has to leave the picture underneath it
    /// untouched.
    #[test]
    fn an_empty_title_is_fully_transparent() {
        let title = Title { text: String::new(), ..Title::default() };
        assert_eq!(ink(&rasterize(&title, 320, 180)), 0);
    }

    /// The block is centred, so the ink has to straddle the middle of the frame
    /// rather than pile up against a corner.
    #[test]
    fn a_single_line_is_centred_in_the_frame() {
        let mask = rasterize(&Title::default(), 640, 360);
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (u32::MAX, 0, u32::MAX, 0);
        for y in 0..mask.height {
            for x in 0..mask.width {
                if at(&mask, x, y) > 0 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        // Within a few pixels of centred on each axis; glyph bearings mean the
        // ink is never exactly symmetric about the pen.
        let cx = (min_x + max_x) as f32 * 0.5;
        let cy = (min_y + max_y) as f32 * 0.5;
        assert!((cx - 320.0).abs() < 12.0, "horizontal centre at {cx}");
        assert!((cy - 180.0).abs() < 12.0, "vertical centre at {cy}");
    }

    /// Two lines have to occupy more height than one, or the second is landing
    /// on top of the first.
    #[test]
    fn each_line_gets_its_own_band() {
        let one = rasterize(&Title { text: "A".into(), ..Title::default() }, 640, 360);
        let two = rasterize(&Title { text: "A\nA".into(), ..Title::default() }, 640, 360);
        let rows =
            |m: &Mask| (0..m.height).filter(|&y| (0..m.width).any(|x| at(m, x, y) > 0)).count();
        assert!(rows(&two) > rows(&one), "the second line landed on the first");
    }

    /// Size is a fraction of the canvas, so a title holds its proportions when
    /// the project is re-mastered rather than shrinking to a caption.
    #[test]
    fn size_scales_with_the_canvas() {
        let small = rasterize(&Title::default(), 640, 360);
        let large = rasterize(&Title::default(), 1280, 720);
        // Ink is an area, so twice the canvas is about four times the coverage.
        let ratio = ink(&large) as f32 / ink(&small) as f32;
        assert!((3.0..5.0).contains(&ratio), "coverage ratio {ratio}");
    }

    #[test]
    fn a_title_is_named_after_its_first_line() {
        let t = Title { text: "Chapter One\nthe beginning".into(), ..Title::default() };
        assert_eq!(t.pool_name(), "Chapter One");
        let blank = Title { text: "  \nsecond".into(), ..Title::default() };
        assert_eq!(blank.pool_name(), PLACEHOLDER);
    }

    /// A title far larger than the frame must clip rather than index outside
    /// the mask — the stamp is the one place that writes at an offset.
    #[test]
    fn a_title_larger_than_the_canvas_is_clipped_not_wrapped() {
        let title = Title { text: "Enormous".into(), size: 4.0, ..Title::default() };
        let mask = rasterize(&title, 64, 64);
        assert_eq!(mask.coverage.len(), 64 * 64);
        assert!(ink(&mask) > 0);
    }
}
