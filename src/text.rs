use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::quad::{Quad, QuadRenderer, Texture};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/ShareTechMono-Regular.ttf");
/// Lucide, subset to the icons the UI actually draws — see
/// `tools/build-icon-font.sh`. An icon font rather than SVGs or PNGs because
/// everything below already rasterizes glyph outlines into an atlas at whatever
/// size is asked for: icons come out crisp at any scale, and cost no new code.
const ICON_FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/lucide-subset.ttf");
/// Sized for HiDPI: glyphs are rasterized at *physical* pixels, so a 2x display
/// asks for double-size bitmaps and quadruple the atlas area. 1024 fit the sizes
/// this UI uses at 1x with room to spare, and would have run the packer close
/// enough to full at 2x that a new size could tip it into the panic below.
const ATLAS_SIZE: u32 = 2048;
const GLYPH_PAD: u32 = 1; // padding between glyphs to prevent bilinear bleed

/// Unicode's Private Use Area, where Lucide puts every icon. The text face has
/// nothing in that range, so a codepoint alone says which face it belongs to.
/// That's why there is no separate icon-drawing path: `draw`, `measure_width`
/// and the rest work on icons unchanged.
const PUA_FIRST: char = '\u{E000}';
const PUA_LAST: char = '\u{F8FF}';

#[derive(Copy, Clone, Debug)]
struct GlyphEntry {
    uv: [f32; 4],
    width: f32,
    height: f32,
    xmin: f32,
    ymin: f32,
    advance: f32,
}

pub struct TextRenderer {
    font: Font,
    icons: Font,
    atlas: Texture,
    /// Physical pixels per logical point, from the window's scale factor.
    ///
    /// Every public size and position here is in logical points, matching the
    /// rest of the UI; this is the one place that knows otherwise. Glyphs are
    /// rasterized at `size * scale` so they carry the display's full detail,
    /// then drawn into a quad divided back down — on a 2x screen that's a
    /// 26px bitmap filling 13 points, i.e. crisp rather than an upscaled 13px
    /// one. The cache is keyed on the physical size, so a window moved between
    /// displays reuses whatever it already has for the new scale.
    scale: f32,
    // Shelf packer state
    cursor_x: u32,
    cursor_y: u32,
    shelf_height: u32,
    glyphs: HashMap<(char, u32), GlyphEntry>,
}

impl TextRenderer {
    pub fn new(device: &wgpu::Device, quads: &QuadRenderer) -> Self {
        let font = Font::from_bytes(FONT_BYTES, FontSettings::default())
            .expect("failed to parse font");
        let icons = Font::from_bytes(ICON_FONT_BYTES, FontSettings::default())
            .expect("failed to parse icon font");
        let atlas = quads.create_empty_texture(
            device,
            ATLAS_SIZE,
            ATLAS_SIZE,
            wgpu::TextureFormat::Rgba8Unorm, // linear: coverage values, not color
        );

        Self {
            font,
            icons,
            atlas,
            scale: 1.0,
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
            glyphs: HashMap::new(),
        }
    }

    /// Set the physical-pixels-per-point ratio. See [`TextRenderer::scale`].
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    /// Which face `ch` comes from. See [`PUA_FIRST`].
    fn face(&self, ch: char) -> &Font {
        if (PUA_FIRST..=PUA_LAST).contains(&ch) {
            &self.icons
        } else {
            &self.font
        }
    }

    /// Push textured glyph quads for `text` into `quads`, with `pos` being the
    /// **baseline** of the first glyph. Position and size are in logical points.
    pub fn draw(
        &mut self,
        queue: &wgpu::Queue,
        quads: &mut QuadRenderer,
        pos: [f32; 2],
        text: &str,
        size_px: f32,
        color: [f32; 4],
    ) {
        // The pen runs in physical pixels: snapping has to land on the grid the
        // display actually has, not the coarser logical one, or half the gain of
        // rasterizing at 2x is thrown away rounding glyphs to even pixels.
        let phys_size = size_px * self.scale;
        let size_key = phys_size.round() as u32;
        let inv = 1.0 / self.scale;
        // Snap baseline to the pixel grid so all glyphs align vertically.
        let baseline_y = (pos[1] * self.scale).round();
        let mut pen_x = pos[0] * self.scale;

        for ch in text.chars() {
            if ch == ' ' {
                let metrics = self.face(ch).metrics(ch, phys_size);
                pen_x += metrics.advance_width;
                continue;
            }
            let entry = match self.glyphs.get(&(ch, size_key)) {
                Some(e) => *e,
                None => self.rasterize_and_upload(queue, ch, phys_size, size_key),
            };

            if entry.width > 0.0 && entry.height > 0.0 {
                // Snap each glyph's top-left to an integer pixel — otherwise the
                // glyph bitmap gets bilinearly blended across screen pixels → blur.
                let x = (pen_x + entry.xmin).round();
                let y = (baseline_y - (entry.ymin + entry.height)).round();
                // Back to points for the quad: the bitmap keeps its physical
                // size on screen because the projection divides by the same
                // scale the rasterizer multiplied by.
                let mut q = Quad::textured([x * inv, y * inv], [
                    entry.width * inv,
                    entry.height * inv,
                ]);
                q.color = color;
                q.uv = entry.uv;
                quads.push_with(q, Some(&self.atlas));
            }
            pen_x += entry.advance;
        }
    }

    /// Ascent (points above baseline) at the given size.
    ///
    /// This and the measurements below all query the font at the physical size
    /// and divide back down, rather than querying at the logical size directly.
    /// The two differ once rasterization rounds a glyph's box to whole pixels,
    /// and layout that disagreed with [`TextRenderer::draw`] about a width is
    /// exactly how text starts overflowing the box drawn to hold it.
    pub fn ascent(&self, size_px: f32) -> f32 {
        let m = self
            .font
            .horizontal_line_metrics(size_px * self.scale)
            .expect("font has no horizontal line metrics");
        m.ascent / self.scale
    }

    /// Descent (points below the baseline) at the given size, as a positive
    /// number — `fontdue` reports it as a negative offset. Paired with
    /// [`TextRenderer::ascent`] when something has to be drawn behind a line of
    /// text rather than beside it.
    pub fn descent(&self, size_px: f32) -> f32 {
        let m = self
            .font
            .horizontal_line_metrics(size_px * self.scale)
            .expect("font has no horizontal line metrics");
        -m.descent / self.scale
    }

    /// Visual bounds of a single glyph: `(height, ymin)`, where `height` is the
    /// rasterized bitmap height and `ymin` is the baseline offset such that the
    /// glyph's top sits at `baseline - (ymin + height)`. Use this — not
    /// `ascent` — when vertically centering a glyph inside a fixed-size box.
    pub fn glyph_visual_bounds(&self, ch: char, size_px: f32) -> (f32, f32) {
        let m = self.face(ch).metrics(ch, size_px * self.scale);
        (m.height as f32 / self.scale, m.ymin as f32 / self.scale)
    }

    /// Pen-advance width of `text` at `size_px`. Cheap — uses font metrics only,
    /// no rasterization or atlas lookups, so it's safe to call every frame for
    /// layout decisions (e.g. right-aligning a timer readout).
    pub fn measure_width(&self, text: &str, size_px: f32) -> f32 {
        text.chars()
            .map(|ch| self.face(ch).metrics(ch, size_px * self.scale).advance_width)
            .sum::<f32>()
            / self.scale
    }

    /// `phys_size` is in physical pixels, and `size_key` is it rounded — the
    /// atlas holds device-resolution bitmaps and knows nothing about points.
    fn rasterize_and_upload(
        &mut self,
        queue: &wgpu::Queue,
        ch: char,
        phys_size: f32,
        size_key: u32,
    ) -> GlyphEntry {
        let (metrics, bitmap) = self.face(ch).rasterize(ch, phys_size);
        let gw = metrics.width as u32;
        let gh = metrics.height as u32;

        let entry = if gw == 0 || gh == 0 {
            GlyphEntry {
                uv: [0.0; 4],
                width: 0.0,
                height: 0.0,
                xmin: metrics.xmin as f32,
                ymin: metrics.ymin as f32,
                advance: metrics.advance_width,
            }
        } else {
            let (ax, ay) = self.allocate(gw, gh);

            // Expand coverage bitmap → RGBA (255, 255, 255, coverage).
            let mut rgba = vec![0u8; (gw * gh * 4) as usize];
            for (i, &cov) in bitmap.iter().enumerate() {
                let o = i * 4;
                rgba[o] = 255;
                rgba[o + 1] = 255;
                rgba[o + 2] = 255;
                rgba[o + 3] = cov;
            }
            self.atlas.write_region(queue, ax, ay, gw, gh, &rgba);

            let atlas = ATLAS_SIZE as f32;
            GlyphEntry {
                uv: [
                    ax as f32 / atlas,
                    ay as f32 / atlas,
                    (ax + gw) as f32 / atlas,
                    (ay + gh) as f32 / atlas,
                ],
                width: gw as f32,
                height: gh as f32,
                xmin: metrics.xmin as f32,
                ymin: metrics.ymin as f32,
                advance: metrics.advance_width,
            }
        };

        self.glyphs.insert((ch, size_key), entry);
        entry
    }

    fn allocate(&mut self, w: u32, h: u32) -> (u32, u32) {
        let padded_w = w + GLYPH_PAD;
        let padded_h = h + GLYPH_PAD;

        if self.cursor_x + padded_w > ATLAS_SIZE {
            // Move to next shelf.
            self.cursor_x = 0;
            self.cursor_y += self.shelf_height;
            self.shelf_height = 0;
        }
        if self.cursor_y + padded_h > ATLAS_SIZE {
            panic!("text atlas full ({}x{})", ATLAS_SIZE, ATLAS_SIZE);
        }

        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += padded_w;
        if padded_h > self.shelf_height {
            self.shelf_height = padded_h;
        }
        pos
    }
}
