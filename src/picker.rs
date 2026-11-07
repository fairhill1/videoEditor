//! The colour picker a title's colour rows open: a saturation-value square
//! and a hue strip for any colour, a hex code to type one, an eyedropper to
//! take one off the screen, the swatches, and the colours picked lately.
//!
//! The colour changes live as the picker is used, and the whole visit is one
//! undo step, closed when the picker is.

use crate::color::{self, from_hex, hsv_to_rgb, rgb_to_hsv, to_hex};
use crate::edit::EditSnapshot;
use crate::quad::Texture;
use crate::layout::{color_chip_rect, color_picker, PickerLayout, PoolTab, TitleControl};
use crate::state::State;
use crate::timeline::SourceId;
use crate::title::TitleColor;

/// The picker while it is open.
pub(crate) struct ColorPicker {
    pub(crate) field: TitleColor,
    /// The title it is picking for. A different clip shown on the effects tab
    /// closes it.
    pub(crate) source: SourceId,
    /// The colour as hue, saturation and value. Kept rather than read back
    /// from the colour, which loses the hue at grey and the saturation at
    /// black: dragging down to black and back up should not snap to red.
    pub(crate) hsv: [f32; 3],
    /// The hex code being typed, while the hex box has the keyboard.
    pub(crate) hex: Option<String>,
    /// Armed: the next press anywhere takes its colour from the screen.
    pub(crate) eyedropper: bool,
    /// Everything as it stood at opening: the undo step the visit becomes.
    before: EditSnapshot,
}

/// The longest hex code the box takes, without the `#`.
const HEX_DIGITS: usize = 6;

impl State {
    /// The picker's layout, while it is open and its row is on screen.
    pub(crate) fn picker_layout(&self) -> Option<PickerLayout> {
        let picker = self.color_picker.as_ref()?;
        if self.pool_tab != PoolTab::Effects {
            return None;
        }
        if self.effects_title().map(|(s, _)| s) != Some(picker.source) {
            return None;
        }
        let row = self
            .effects_panel()
            .title
            .iter()
            .find(|r| r.control == TitleControl::Color(picker.field))?
            .row;
        Some(color_picker(color_chip_rect(row), self.logical_size()))
    }

    pub(crate) fn open_color_picker(&mut self, field: TitleColor) {
        self.close_color_picker();
        self.finish_title_edit();
        self.font_menu = None;
        let Some((source, title)) = self.effects_title() else {
            return;
        };
        let rgb = field.get(title);
        self.color_picker = Some(ColorPicker {
            field,
            source,
            hsv: rgb_to_hsv([rgb[0], rgb[1], rgb[2]]),
            hex: None,
            eyedropper: false,
            before: self.edit_snapshot(),
        });
    }

    /// Close the picker, keeping the colour: one undo step if it changed,
    /// and the colour at the front of the recent ones.
    pub(crate) fn close_color_picker(&mut self) {
        let Some(picker) = self.color_picker.take() else {
            return;
        };
        if picker.before == self.edit_snapshot() {
            return;
        }
        self.push_step(picker.before);
        if let Some(color) = self.media.title(picker.source).map(|t| picker.field.get(t)) {
            color::remember(&mut self.recent_colors, color);
            color::save_recent(&self.recent_colors);
        }
    }

    /// The colour the picker is showing, as the title has it.
    pub(crate) fn picker_color(&self) -> Option<[f32; 4]> {
        let picker = self.color_picker.as_ref()?;
        self.media.title(picker.source).map(|t| picker.field.get(t))
    }

    /// Give the title `rgb`, keeping its alpha.
    fn apply_picker_rgb(&mut self, rgb: [f32; 3]) {
        let Some(picker) = self.color_picker.as_ref() else {
            return;
        };
        let (field, source) = (picker.field, picker.source);
        let Some(mut title) = self.media.title(source).cloned() else {
            return;
        };
        let alpha = field.get(&title)[3];
        field.set(&mut title, [rgb[0], rgb[1], rgb[2], alpha]);
        self.media.set_title(source, title);
    }

    /// Take a whole colour from somewhere other than the square: a swatch,
    /// a hex code, the screen.
    pub(crate) fn pick_color(&mut self, c: [f32; 4]) {
        let Some(picker) = self.color_picker.as_mut() else {
            return;
        };
        let [h, s, v] = rgb_to_hsv([c[0], c[1], c[2]]);
        // A grey says nothing about hue, and black nothing about saturation
        // either: the square keeps what it had for those.
        picker.hsv = [
            if s > 0.0 && v > 0.0 { h } else { picker.hsv[0] },
            if v > 0.0 { s } else { picker.hsv[1] },
            v,
        ];
        self.apply_picker_rgb([c[0], c[1], c[2]]);
    }

    /// Set the colour from the square at the cursor: saturation across,
    /// value down.
    pub(crate) fn drag_picker_sv(&mut self) {
        let (Some(layout), Some(picker)) = (self.picker_layout(), self.color_picker.as_mut()) else {
            return;
        };
        let r = layout.sv;
        picker.hsv[1] = ((self.cursor[0] - r.x) / r.w).clamp(0.0, 1.0);
        picker.hsv[2] = 1.0 - ((self.cursor[1] - r.y) / r.h).clamp(0.0, 1.0);
        picker.hex = None;
        let rgb = hsv_to_rgb(picker.hsv);
        self.apply_picker_rgb(rgb);
    }

    /// Set the hue from the strip at the cursor, top to bottom.
    pub(crate) fn drag_picker_hue(&mut self) {
        let (Some(layout), Some(picker)) = (self.picker_layout(), self.color_picker.as_mut()) else {
            return;
        };
        let r = layout.hue;
        // Short of 1 at the bottom, which is red again and would jump the
        // marker back to the top on the next opening.
        picker.hsv[0] = ((self.cursor[1] - r.y) / r.h).clamp(0.0, 0.9999);
        picker.hex = None;
        let rgb = hsv_to_rgb(picker.hsv);
        self.apply_picker_rgb(rgb);
    }

    /// Start typing a hex code, beginning from the colour as it is.
    pub(crate) fn begin_hex_edit(&mut self) {
        let current = self.picker_color();
        if let (Some(picker), Some(c)) = (self.color_picker.as_mut(), current) {
            if picker.hex.is_some() {
                return;
            }
            picker.hex = Some(to_hex(c));
            picker.eyedropper = false;
        }
    }

    pub(crate) fn typing_hex(&self) -> bool {
        self.color_picker.as_ref().is_some_and(|p| p.hex.is_some())
    }

    /// Type into the hex box. Whatever is not a hex digit is left out, and a
    /// complete code takes effect at once.
    pub(crate) fn type_hex(&mut self, text: &str) {
        let Some(hex) = self.color_picker.as_mut().and_then(|p| p.hex.as_mut()) else {
            return;
        };
        for c in text.chars().filter(char::is_ascii_hexdigit) {
            if hex.len() < HEX_DIGITS {
                hex.push(c.to_ascii_uppercase());
            }
        }
        self.apply_typed_hex();
    }

    pub(crate) fn backspace_hex(&mut self) {
        if let Some(hex) = self.color_picker.as_mut().and_then(|p| p.hex.as_mut()) {
            hex.pop();
        }
        self.apply_typed_hex();
    }

    /// Stop typing in the hex box. A short code like `F80` counts now; one
    /// that makes no colour leaves the colour as it was.
    pub(crate) fn finish_hex_edit(&mut self) {
        let Some(code) = self.color_picker.as_mut().and_then(|p| p.hex.take()) else {
            return;
        };
        if let Some(c) = from_hex(&code) {
            self.pick_color(c);
        }
    }

    fn apply_typed_hex(&mut self) {
        let code = self.color_picker.as_ref().and_then(|p| p.hex.clone());
        // Three digits would be a colour too, but taking it would flash the
        // title through a shorthand on the way to typing all six.
        if let Some(c) = code.filter(|h| h.len() == HEX_DIGITS).and_then(|h| from_hex(&h)) {
            self.pick_color(c);
        }
    }

    /// Arm or disarm the eyedropper.
    pub(crate) fn toggle_eyedropper(&mut self) {
        if !self.surface_copy_src {
            return;
        }
        if let Some(picker) = self.color_picker.as_mut() {
            picker.eyedropper = !picker.eyedropper;
            picker.hex = None;
        }
    }

    pub(crate) fn eyedropper_armed(&self) -> bool {
        self.color_picker.as_ref().is_some_and(|p| p.eyedropper)
    }

    /// The press the eyedropper was armed for: ask the next frame for the
    /// pixel under the cursor.
    pub(crate) fn eyedrop_at_cursor(&mut self) {
        if let Some(picker) = self.color_picker.as_mut() {
            picker.eyedropper = false;
        }
        let x = (self.cursor[0] * self.scale).floor().max(0.0) as u32;
        let y = (self.cursor[1] * self.scale).floor().max(0.0) as u32;
        if x < self.size.width && y < self.size.height {
            self.eyedropper_pick = Some([x, y]);
        }
    }
}

/// The picker's two gradients, as pictures: the hue strip, which never
/// changes, and the square for one hue, redrawn when the hue moves.
pub(crate) struct PickerTextures {
    pub(crate) hue: Texture,
    pub(crate) sv: Texture,
    sv_hue: f32,
}

/// Texels along each side of the square. Sampled smoothly, so a gradient
/// needs few: these are under two points apart at the size it is drawn.
pub(crate) const SV_TEXELS: u32 = 64;
pub(crate) const HUE_TEXELS: u32 = 256;

fn byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The square for `hue`: saturation across, value down, as sRGB bytes.
fn sv_pixels(hue: f32) -> Vec<u8> {
    let n = SV_TEXELS;
    let last = (n - 1) as f32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for j in 0..n {
        for i in 0..n {
            let [r, g, b] = hsv_to_rgb([hue, i as f32 / last, 1.0 - j as f32 / last]);
            px.extend_from_slice(&[byte(r), byte(g), byte(b), 255]);
        }
    }
    px
}

impl State {
    /// The gradients for the picker's hue as it stands, made or redrawn as
    /// needed.
    pub(crate) fn picker_textures(&mut self) -> Option<&PickerTextures> {
        let hue = self.color_picker.as_ref()?.hsv[0];
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        if self.picker_tex.is_none() {
            let hue_tex = self.quads.create_empty_texture(&self.device, 1, HUE_TEXELS, format);
            let mut px = Vec::with_capacity((HUE_TEXELS * 4) as usize);
            for j in 0..HUE_TEXELS {
                let [r, g, b] = hsv_to_rgb([j as f32 / HUE_TEXELS as f32, 1.0, 1.0]);
                px.extend_from_slice(&[byte(r), byte(g), byte(b), 255]);
            }
            hue_tex.write_region(&self.queue, 0, 0, 1, HUE_TEXELS, &px);
            let sv = self.quads.create_empty_texture(&self.device, SV_TEXELS, SV_TEXELS, format);
            sv.write_region(&self.queue, 0, 0, SV_TEXELS, SV_TEXELS, &sv_pixels(hue));
            self.picker_tex = Some(PickerTextures { hue: hue_tex, sv, sv_hue: hue });
        }
        let tex = self.picker_tex.as_mut()?;
        if tex.sv_hue != hue {
            tex.sv.write_region(&self.queue, 0, 0, SV_TEXELS, SV_TEXELS, &sv_pixels(hue));
            tex.sv_hue = hue;
        }
        self.picker_tex.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_square_runs_white_to_the_hue_across_and_to_black_down() {
        let px = sv_pixels(0.0);
        let at = |i: u32, j: u32| {
            let k = ((j * SV_TEXELS + i) * 4) as usize;
            [px[k], px[k + 1], px[k + 2]]
        };
        assert_eq!(at(0, 0), [255, 255, 255]);
        assert_eq!(at(SV_TEXELS - 1, 0), [255, 0, 0]);
        assert_eq!(at(0, SV_TEXELS - 1), [0, 0, 0]);
        assert_eq!(at(SV_TEXELS - 1, SV_TEXELS - 1), [0, 0, 0]);
    }
}
