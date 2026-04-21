use std::collections::HashMap;

use fontdue::{Font, FontSettings};

use crate::quad::{Quad, QuadRenderer, Texture};

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/ShareTechMono-Regular.ttf");
const ATLAS_SIZE: u32 = 1024;
const GLYPH_PAD: u32 = 1; // padding between glyphs to prevent bilinear bleed

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
    atlas: Texture,
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
        let atlas = quads.create_empty_texture(
            device,
            ATLAS_SIZE,
            ATLAS_SIZE,
            wgpu::TextureFormat::Rgba8Unorm, // linear: coverage values, not color
        );

        Self {
            font,
            atlas,
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
            glyphs: HashMap::new(),
        }
    }

    /// Push textured glyph quads for `text` into `quads`, with `pos` being the
    /// **baseline** of the first glyph.
    pub fn draw(
        &mut self,
        queue: &wgpu::Queue,
        quads: &mut QuadRenderer,
        pos: [f32; 2],
        text: &str,
        size_px: f32,
        color: [f32; 4],
    ) {
        let size_key = size_px.round() as u32;
        // Snap baseline to the pixel grid so all glyphs align vertically.
        let baseline_y = pos[1].round();
        let mut pen_x = pos[0];

        for ch in text.chars() {
            if ch == ' ' {
                let metrics = self.font.metrics(ch, size_px);
                pen_x += metrics.advance_width;
                continue;
            }
            let entry = match self.glyphs.get(&(ch, size_key)) {
                Some(e) => *e,
                None => self.rasterize_and_upload(queue, ch, size_px, size_key),
            };

            if entry.width > 0.0 && entry.height > 0.0 {
                // Snap each glyph's top-left to an integer pixel — otherwise the
                // glyph bitmap gets bilinearly blended across screen pixels → blur.
                let x = (pen_x + entry.xmin).round();
                let y = (baseline_y - (entry.ymin + entry.height)).round();
                let mut q = Quad::textured([x, y], [entry.width, entry.height]);
                q.color = color;
                q.uv = entry.uv;
                quads.push_with(q, Some(&self.atlas));
            }
            pen_x += entry.advance;
        }
    }

    /// Height of one line in pixels at the given size. Useful for vertical stacking.
    pub fn line_height(&self, size_px: f32) -> f32 {
        let m = self
            .font
            .horizontal_line_metrics(size_px)
            .expect("font has no horizontal line metrics");
        m.new_line_size
    }

    /// Ascent (pixels above baseline) at the given size.
    pub fn ascent(&self, size_px: f32) -> f32 {
        let m = self
            .font
            .horizontal_line_metrics(size_px)
            .expect("font has no horizontal line metrics");
        m.ascent
    }

    fn rasterize_and_upload(
        &mut self,
        queue: &wgpu::Queue,
        ch: char,
        size_px: f32,
        size_key: u32,
    ) -> GlyphEntry {
        let (metrics, bitmap) = self.font.rasterize(ch, size_px);
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
