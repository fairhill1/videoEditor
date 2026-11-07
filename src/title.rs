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

use serde::{Deserialize, Serialize};

use crate::effects::{blur_plane, BlurPlan, ParamSpec, Plane, Unit};
use crate::fonts;
use crate::still::Rgba;

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
    /// An installed family by name, or `None` for the UI's own face. Kept by
    /// name rather than by file so a project still finds the font after it is
    /// updated or reinstalled; see [`crate::fonts::face`] for a missing one.
    #[serde(default)]
    pub font: Option<String>,
    /// How heavy the letters are, 100 (thin) to 900 (black). The font draws
    /// the nearest weight it has.
    #[serde(default = "regular")]
    pub weight: u16,
    #[serde(default)]
    pub outline: Outline,
    #[serde(default)]
    pub shadow: Shadow,
    /// The distance from one line to the next, as a multiple of the font's
    /// own: 1 is the face as designed, with a little air.
    #[serde(default = "normal_spacing")]
    pub line_spacing: f32,
}

fn normal_spacing() -> f32 {
    1.0
}

fn regular() -> u16 {
    fonts::REGULAR
}

/// A line of colour around every letter.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outline {
    pub enabled: bool,
    /// How far it reaches out from the letters, as a fraction of the canvas
    /// height, like the title's own size.
    pub width: f32,
    pub color: [f32; 4],
}

impl Default for Outline {
    fn default() -> Self {
        Self { enabled: false, width: 0.004, color: [0.0, 0.0, 0.0, 1.0] }
    }
}

/// A soft dark copy of the letters, offset down and to the right — the light
/// falling from the upper left, as it does on everything else in the UI.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shadow {
    pub enabled: bool,
    /// How far the shadow falls, as a fraction of the canvas height.
    pub distance: f32,
    /// How soft its edge is: a blur radius, as a fraction of the canvas height.
    pub softness: f32,
    pub opacity: f32,
}

impl Default for Shadow {
    fn default() -> Self {
        Self { enabled: false, distance: 0.006, softness: 0.006, opacity: 0.7 }
    }
}

/// Moving a text cursor through a title's text. Positions are byte offsets
/// on character boundaries, which is what `String::insert_str` takes; columns
/// count characters, so moving up and down keeps to the same letter count
/// along the line.
pub mod caret {
    /// The start of the character before `i`.
    pub fn left(text: &str, i: usize) -> usize {
        text[..i].char_indices().next_back().map_or(0, |(j, _)| j)
    }

    /// The end of the character after `i`.
    pub fn right(text: &str, i: usize) -> usize {
        text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    /// Which line `i` is on, and how many characters along it.
    pub fn line_col(text: &str, i: usize) -> (usize, usize) {
        let before = &text[..i];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map_or(0, |n| n + 1);
        (line, text[start..i].chars().count())
    }

    /// The position `col` characters along line `line`, held to the line's
    /// end, and to the last line.
    pub fn at(text: &str, line: usize, col: usize) -> usize {
        let mut start = 0;
        for (n, l) in text.split('\n').enumerate() {
            if n == line {
                return start + l.char_indices().nth(col).map_or(l.len(), |(j, _)| j);
            }
            start += l.len() + 1;
        }
        text.len()
    }

    /// The same column on the line above, or the start of the text from the
    /// first line.
    pub fn up(text: &str, i: usize) -> usize {
        match line_col(text, i) {
            (0, _) => 0,
            (line, col) => at(text, line - 1, col),
        }
    }

    /// The same column on the line below, or the end of the text from the
    /// last line.
    pub fn down(text: &str, i: usize) -> usize {
        let (line, col) = line_col(text, i);
        if line + 1 >= text.split('\n').count() {
            return text.len();
        }
        at(text, line + 1, col)
    }

    pub fn home(text: &str, i: usize) -> usize {
        at(text, line_col(text, i).0, 0)
    }

    pub fn end(text: &str, i: usize) -> usize {
        at(text, line_col(text, i).0, usize::MAX)
    }
}

/// The colours the panel offers, as swatches: a row of neutrals and warm
/// hues, then a row of cool ones, around the wheel.
pub const SWATCHES: [[f32; 4]; 16] = [
    [1.0, 1.0, 1.0, 1.0],
    [0.75, 0.75, 0.77, 1.0],
    [0.45, 0.45, 0.48, 1.0],
    [0.0, 0.0, 0.0, 1.0],
    [0.92, 0.22, 0.20, 1.0],
    [1.0, 0.56, 0.10, 1.0],
    [1.0, 0.84, 0.12, 1.0],
    [0.98, 0.93, 0.70, 1.0],
    [0.62, 0.86, 0.22, 1.0],
    [0.22, 0.76, 0.34, 1.0],
    [0.10, 0.70, 0.66, 1.0],
    [0.30, 0.82, 0.95, 1.0],
    [0.20, 0.48, 0.96, 1.0],
    [0.52, 0.34, 0.92, 1.0],
    [0.88, 0.30, 0.82, 1.0],
    [1.0, 0.62, 0.74, 1.0],
];

/// A number on a title the panel has a slider for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TitleNumber {
    Size,
    LineSpacing,
    OutlineWidth,
    ShadowDistance,
    ShadowSoftness,
    ShadowOpacity,
}

impl TitleNumber {
    pub fn spec(self) -> ParamSpec {
        let (name, min, max, default, unit) = match self {
            Self::Size => ("Size", 0.02, 0.4, DEFAULT_SIZE, Unit::CanvasHeight),
            Self::LineSpacing => ("Line spacing", 0.5, 3.0, 1.0, Unit::Percent),
            Self::OutlineWidth => ("Thickness", 0.0, 0.02, 0.004, Unit::CanvasHeight),
            Self::ShadowDistance => ("Distance", 0.0, 0.04, 0.006, Unit::CanvasHeight),
            Self::ShadowSoftness => ("Softness", 0.0, 0.03, 0.006, Unit::CanvasHeight),
            Self::ShadowOpacity => ("Opacity", 0.0, 1.0, 0.7, Unit::Percent),
        };
        ParamSpec { name, min, max, default, unit }
    }

    pub fn get(self, title: &Title) -> f32 {
        match self {
            Self::Size => title.size,
            Self::LineSpacing => title.line_spacing,
            Self::OutlineWidth => title.outline.width,
            Self::ShadowDistance => title.shadow.distance,
            Self::ShadowSoftness => title.shadow.softness,
            Self::ShadowOpacity => title.shadow.opacity,
        }
    }

    pub fn set(self, title: &mut Title, value: f32) {
        let spec = self.spec();
        let value = value.clamp(spec.min, spec.max);
        match self {
            Self::Size => title.size = value,
            Self::LineSpacing => title.line_spacing = value,
            Self::OutlineWidth => title.outline.width = value,
            Self::ShadowDistance => title.shadow.distance = value,
            Self::ShadowSoftness => title.shadow.softness = value,
            Self::ShadowOpacity => title.shadow.opacity = value,
        }
    }
}

/// A colour on a title the panel has swatches for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TitleColor {
    Fill,
    Outline,
}

impl TitleColor {
    pub fn get(self, title: &Title) -> [f32; 4] {
        match self {
            Self::Fill => title.color,
            Self::Outline => title.outline.color,
        }
    }

    pub fn set(self, title: &mut Title, color: [f32; 4]) {
        match self {
            Self::Fill => title.color = color,
            Self::Outline => title.outline.color = color,
        }
    }
}

/// A part of a title's styling that can be switched on and off.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TitlePart {
    Outline,
    Shadow,
}

impl TitlePart {
    pub fn name(self) -> &'static str {
        match self {
            Self::Outline => "Outline",
            Self::Shadow => "Drop shadow",
        }
    }

    pub fn enabled(self, title: &Title) -> bool {
        match self {
            Self::Outline => title.outline.enabled,
            Self::Shadow => title.shadow.enabled,
        }
    }

    pub fn set(self, title: &mut Title, on: bool) {
        match self {
            Self::Outline => title.outline.enabled = on,
            Self::Shadow => title.shadow.enabled = on,
        }
    }
}

impl Default for Title {
    fn default() -> Self {
        Self {
            text: PLACEHOLDER.to_string(),
            size: DEFAULT_SIZE,
            color: [1.0, 1.0, 1.0, 1.0],
            font: None,
            weight: fonts::REGULAR,
            outline: Outline::default(),
            shadow: Shadow::default(),
            line_spacing: 1.0,
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
    let picked = fonts::face(title.font.as_deref(), title.weight);
    let fallback = fonts::face(None, fonts::REGULAR);
    // A letter the picked face does not have comes from the default one,
    // rather than as the face's empty box.
    let face_for = |ch: char| {
        if ch.is_whitespace() || picked.lookup_glyph_index(ch) != 0 {
            picked.as_ref()
        } else {
            fallback.as_ref()
        }
    };
    let font = picked.as_ref();
    let metrics = font
        .horizontal_line_metrics(px)
        .expect("font has no horizontal line metrics");
    let line_h = (metrics.ascent - metrics.descent) * (1.0 + LINE_GAP) * title.line_spacing;

    // Split rather than `lines`, which drops a blank last line: a line left
    // empty at the end is space the text box shows, so the title keeps it.
    let lines: Vec<&str> = title.text.split('\n').collect();
    let block_h = line_h * lines.len() as f32;
    // The first baseline: the block centred, then down by one ascent to get
    // from the top of the first line to the line it sits on.
    let mut baseline = (height as f32 - block_h) * 0.5 + metrics.ascent;

    for line in lines {
        let line_w: f32 = line.chars().map(|ch| face_for(ch).metrics(ch, px).advance_width).sum();
        let mut pen = (width as f32 - line_w) * 0.5;
        for ch in line.chars() {
            let (m, bitmap) = face_for(ch).rasterize(ch, px);
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

/// Draw `title` as a picture: its shadow, then its outline, then its letters,
/// each in its own colour, over transparency. Straight RGBA at `width x height`,
/// which is what an image is, so the preview and the export both take a title
/// the way they take a PNG.
///
/// The styling only ever touches the box around the ink, grown by however far
/// the outline and shadow reach; the rest of the canvas is left transparent
/// without being looked at, which is most of it for most titles.
pub fn render(title: &Title, width: u32, height: u32) -> Rgba {
    let mask = rasterize(title, width, height);
    // What colour the transparent pixels carry. They are never seen, but they
    // are not never used: a filtered sample at the edge of the letters and the
    // export's chroma, one sample per 2x2 pixels, both average across them.
    // Giving them the colour of the outermost layer keeps that edge its own
    // colour rather than fringing it towards black.
    let bleed = if title.shadow.enabled {
        [0.0; 3]
    } else if title.outline.enabled {
        let [r, g, b, _] = title.outline.color;
        [r, g, b]
    } else {
        let [r, g, b, _] = title.color;
        [r, g, b]
    };
    let bleed = bleed.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8);
    let mut out = Rgba {
        width,
        height,
        pixels: [bleed[0], bleed[1], bleed[2], 0].repeat(width as usize * height as usize),
    };
    let Some([x0, y0, x1, y1]) = ink_box(&mask) else {
        return out;
    };
    let h = height as f32;
    let outline_px = if title.outline.enabled { title.outline.width * h } else { 0.0 };
    let shadow = title.shadow.enabled.then(|| {
        let d = title.shadow.distance * h * std::f32::consts::FRAC_1_SQRT_2;
        (d.round() as i32, title.shadow.softness * h, title.shadow.opacity)
    });
    let reach = outline_px.ceil() as i32
        + shadow.map_or(0, |(d, soft, _)| d.abs() + (soft * 3.0).ceil() as i32)
        + 2;
    let bx0 = (x0 as i32 - reach).max(0) as usize;
    let by0 = (y0 as i32 - reach).max(0) as usize;
    let bx1 = (x1 as i32 + reach).min(width as i32 - 1) as usize;
    let by1 = (y1 as i32 + reach).min(height as i32 - 1) as usize;
    let (bw, bh) = (bx1 - bx0 + 1, by1 - by0 + 1);

    let mut fill = Plane::new(bw, bh);
    for y in 0..bh {
        for x in 0..bw {
            fill.data[y * bw + x] =
                mask.coverage[(by0 + y) * width as usize + bx0 + x] as f32 / 255.0;
        }
    }
    // The letters with the outline grown around them: the silhouette the
    // outline colour fills, and the shape the shadow is cast by.
    let shape = if outline_px > 0.0 { grow(&fill, outline_px) } else { fill.clone() };
    let cast = shadow.map(|(d, soft, opacity)| {
        let soft = BlurPlan::for_sigma(soft).map_or_else(|| shape.clone(), |p| blur_plane(&shape, p));
        // Blurred at a reduced size when it is very soft; the shadow only has
        // to be read back at the box's own size, so sample it there.
        (soft, d, opacity)
    });

    for y in 0..bh {
        for x in 0..bw {
            let i = y * bw + x;
            // Bottom to top: shadow, outline, letters — each laid over what
            // is below it.
            let mut px = [0.0f32; 4];
            let o = ((by0 + y) * width as usize + bx0 + x) * 4;
            if let Some((soft, d, opacity)) = &cast {
                let a = sample(soft, bw, bh, x as i32 - d, y as i32 - d) * opacity;
                px = over(px, [0.0, 0.0, 0.0, a]);
            }
            if outline_px > 0.0 {
                let [r, g, b, a] = title.outline.color;
                px = over(px, [r, g, b, a * shape.data[i]]);
            }
            let [r, g, b, a] = title.color;
            px = over(px, [r, g, b, a * fill.data[i]]);
            if px[3] <= 0.0 {
                continue;
            }
            for c in 0..4 {
                out.pixels[o + c] = (px[c].clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }
    out
}

/// The smallest box holding every inked pixel, as inclusive `[x0, y0, x1, y1]`.
fn ink_box(mask: &Mask) -> Option<[u32; 4]> {
    let mut b: Option<[u32; 4]> = None;
    for y in 0..mask.height {
        for x in 0..mask.width {
            if mask.coverage[(y * mask.width + x) as usize] == 0 {
                continue;
            }
            b = Some(match b {
                None => [x, y, x, y],
                Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
            });
        }
    }
    b
}

/// A blurred plane read back at a box position, where the blur may have run
/// on a smaller copy of it: the work picture is scaled to fit the box.
fn sample(p: &Plane, bw: usize, bh: usize, x: i32, y: i32) -> f32 {
    if x < 0 || y < 0 || x >= bw as i32 || y >= bh as i32 {
        return 0.0;
    }
    let sx = ((x as f32 + 0.5) * p.w as f32 / bw as f32 - 0.5).clamp(0.0, (p.w - 1) as f32);
    let sy = ((y as f32 + 0.5) * p.h as f32 / bh as f32 - 0.5).clamp(0.0, (p.h - 1) as f32);
    let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
    let (fx, fy) = (sx - ix as f32, sy - iy as f32);
    let (jx, jy) = ((ix + 1).min(p.w - 1), (iy + 1).min(p.h - 1));
    let at = |x: usize, y: usize| p.data[y * p.w + x];
    let top = at(ix, iy) * (1.0 - fx) + at(jx, iy) * fx;
    let bottom = at(ix, jy) * (1.0 - fx) + at(jx, jy) * fx;
    top * (1.0 - fy) + bottom * fy
}

/// `top` laid over `under`, both straight RGBA.
fn over(under: [f32; 4], top: [f32; 4]) -> [f32; 4] {
    let a = top[3] + under[3] * (1.0 - top[3]);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let mut out = [0.0, 0.0, 0.0, a];
    for c in 0..3 {
        out[c] = (top[c] * top[3] + under[c] * under[3] * (1.0 - top[3])) / a;
    }
    out
}

/// The shape grown outwards by `r` pixels with round corners: a pixel is
/// covered by how far inside `r` of the letters it lies, softened over one
/// pixel so the outline's own edge is antialiased.
fn grow(fill: &Plane, r: f32) -> Plane {
    let (w, h) = (fill.w, fill.h);
    // Squared distance to the nearest pixel at least half inked.
    let far = ((w * w + h * h) as f32).max(1.0);
    let mut d: Vec<f32> = fill.data.iter().map(|&c| if c >= 0.5 { 0.0 } else { far }).collect();
    let mut line = Vec::new();
    for y in 0..h {
        line.clear();
        line.extend_from_slice(&d[y * w..(y + 1) * w]);
        let out = distance_1d(&line);
        d[y * w..(y + 1) * w].copy_from_slice(&out);
    }
    for x in 0..w {
        line.clear();
        line.extend((0..h).map(|y| d[y * w + x]));
        let out = distance_1d(&line);
        for y in 0..h {
            d[y * w + x] = out[y];
        }
    }
    let mut grown = Plane::new(w, h);
    for i in 0..w * h {
        let reach = (r + 0.5 - d[i].sqrt()).clamp(0.0, 1.0);
        grown.data[i] = reach.max(fill.data[i]);
    }
    grown
}

/// One pass of the exact squared Euclidean distance transform (Felzenszwalb
/// and Huttenlocher): for each sample, the least of `f[q] + (p - q)^2` over
/// every `q`, by the lower envelope of the parabolas rooted at each sample.
fn distance_1d(f: &[f32]) -> Vec<f32> {
    let n = f.len();
    let mut out = vec![0.0; n];
    if n == 0 {
        return out;
    }
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    let mut k = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    let cross = |q: usize, p: usize| {
        ((f[q] + (q * q) as f32) - (f[p] + (p * p) as f32)) / (2.0 * q as f32 - 2.0 * p as f32)
    };
    for q in 1..n {
        let mut s = cross(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = cross(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, o) in out.iter_mut().enumerate() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let dq = q as f32 - v[k] as f32;
        *o = dq * dq + f[v[k]];
    }
    out
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

    fn alpha(img: &Rgba, x: u32, y: u32) -> u8 {
        img.pixels[((y * img.width + x) * 4 + 3) as usize]
    }

    fn rgb(img: &Rgba, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * img.width + x) * 4) as usize;
        [img.pixels[i], img.pixels[i + 1], img.pixels[i + 2]]
    }

    fn opaque(img: &Rgba) -> usize {
        img.pixels.chunks(4).filter(|p| p[3] > 0).count()
    }

    /// A bar of solid ink: an `I`, which covers whole rows of pixels with a
    /// clear edge on either side to look just beyond.
    fn bar() -> Title {
        Title { text: "I".into(), size: 0.5, color: [1.0, 0.0, 0.0, 1.0], ..Title::default() }
    }

    /// The middle row's first and last fully inked columns.
    fn bar_edges(title: &Title, w: u32, h: u32) -> (u32, u32) {
        let mask = rasterize(title, w, h);
        let row = h / 2;
        let inked: Vec<u32> = (0..w).filter(|&x| at(&mask, x, row) == 255).collect();
        (inked[0], *inked.last().unwrap())
    }

    #[test]
    fn a_plain_title_is_its_colour_on_transparency() {
        let title = bar();
        let img = render(&title, 320, 180);
        let (left, _) = bar_edges(&title, 320, 180);
        assert_eq!(alpha(&img, left, 90), 255);
        assert_eq!(rgb(&img, left, 90), [255, 0, 0]);
        assert_eq!(alpha(&img, 0, 0), 0);
        assert_eq!(opaque(&img), ink(&rasterize(&title, 320, 180)));
    }

    #[test]
    fn an_outline_rings_the_letters_in_its_own_colour() {
        let plain = bar();
        let outlined = Title {
            outline: Outline { enabled: true, width: 4.0 / 180.0, color: [0.0, 0.0, 1.0, 1.0] },
            ..bar()
        };
        let (left, right) = bar_edges(&plain, 320, 180);
        let img = render(&outlined, 320, 180);
        // Three pixels out from the ink is inside a four pixel outline.
        for x in [left - 3, right + 3] {
            assert_eq!(alpha(&img, x, 90), 255, "x {x}");
            assert_eq!(rgb(&img, x, 90), [0, 0, 255], "x {x}");
        }
        // Well past it is still clear, and the letters keep their colour.
        assert_eq!(alpha(&img, left - 8, 90), 0);
        assert_eq!(rgb(&img, (left + right) / 2, 90), [255, 0, 0]);
    }

    #[test]
    fn a_shadow_falls_down_and_to_the_right() {
        let plain = bar();
        let shadowed = Title {
            shadow: Shadow { enabled: true, distance: 8.0 / 180.0, softness: 0.0, opacity: 1.0 },
            ..bar()
        };
        let (left, right) = bar_edges(&plain, 320, 180);
        let img = render(&shadowed, 320, 180);
        // The offset is diagonal, about 6 px across and 6 down.
        assert!(alpha(&img, right + 4, 90) > 200, "nothing right of the bar");
        assert_eq!(rgb(&img, right + 4, 90), [0, 0, 0]);
        assert_eq!(alpha(&img, left - 4, 90), 0, "shadow on the lit side");
    }

    #[test]
    fn a_soft_shadow_fades_out_rather_than_stopping() {
        let shadowed = Title {
            shadow: Shadow { enabled: true, distance: 8.0 / 180.0, softness: 3.0 / 180.0, opacity: 1.0 },
            ..bar()
        };
        let (_, right) = bar_edges(&bar(), 320, 180);
        let img = render(&shadowed, 320, 180);
        let partial = (right + 1..right + 16)
            .filter(|&x| (1..255).contains(&alpha(&img, x, 90)))
            .count();
        assert!(partial >= 4, "only {partial} soft pixels at the edge");
    }

    #[test]
    fn the_caret_walks_letters_and_lines() {
        let text = "ab\ncdé\n\nx";
        assert_eq!(caret::left(text, 3), 2);
        assert_eq!(caret::right(text, 5), 7, "é is two bytes");
        assert_eq!(caret::line_col(text, 7), (1, 3));
        // Down from the end of "cdé" onto the blank line, held to its end,
        // then onto the start of "x".
        assert_eq!(caret::down(text, 7), 8);
        assert_eq!(caret::down(text, 8), 9);
        assert_eq!(caret::up(text, 10), 8);
        assert_eq!(caret::up(text, 1), 0);
        assert_eq!(caret::down(text, 9), text.len());
        assert_eq!(caret::home(text, 5), 3);
        assert_eq!(caret::end(text, 4), 7);
        assert_eq!(caret::at(text, 9, 0), text.len());
    }

    #[test]
    fn the_distance_transform_is_exact() {
        let far = 1e6;
        let f = [far, 0.0, far, far, far, 0.0, far, far];
        let got = distance_1d(&f);
        for (p, g) in got.iter().enumerate() {
            let want = (0..f.len())
                .map(|q| f[q] + ((p as f32 - q as f32).powi(2)))
                .fold(f32::INFINITY, f32::min);
            assert_eq!(*g, want, "at {p}");
        }
    }

    #[test]
    fn line_spacing_spreads_the_lines_apart() {
        let rows = |spacing: f32| {
            let title = Title { text: "I\nI".into(), line_spacing: spacing, ..Title::default() };
            let mask = rasterize(&title, 320, 360);
            let w = mask.width as usize;
            let inked: Vec<usize> =
                (0..mask.height as usize).filter(|y| mask.coverage[y * w..(y + 1) * w].iter().any(|&c| c > 128)).collect();
            inked.last().unwrap() - inked.first().unwrap()
        };
        assert!(rows(2.0) > rows(1.0) + 30, "{} vs {}", rows(2.0), rows(1.0));
        assert!(rows(0.6) < rows(1.0));
    }

    #[test]
    fn an_empty_title_renders_fully_transparent() {
        let title = Title {
            text: String::new(),
            outline: Outline { enabled: true, ..Outline::default() },
            shadow: Shadow { enabled: true, ..Shadow::default() },
            ..Title::default()
        };
        assert_eq!(opaque(&render(&title, 320, 180)), 0);
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
