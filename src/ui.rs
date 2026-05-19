use crate::quad::{Quad, QuadRenderer};
use crate::text::TextRenderer;

#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, [px, py]: [f32; 2]) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

const BTN_BG: [f32; 4] = [0.18, 0.18, 0.22, 0.92];
const BTN_BG_HOVER: [f32; 4] = [0.30, 0.30, 0.36, 0.95];
const BTN_LABEL: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const TOOLTIP_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.88];
const TOOLTIP_LABEL: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const TOOLTIP_GAP: f32 = 6.0;
const TOOLTIP_PAD_X: f32 = 7.0;
const TOOLTIP_PAD_Y: f32 = 4.0;

pub fn draw_button(
    quads: &mut QuadRenderer,
    text: &mut TextRenderer,
    queue: &wgpu::Queue,
    rect: Rect,
    label: &str,
    label_size: f32,
    hovered: bool,
) {
    let bg = if hovered { BTN_BG_HOVER } else { BTN_BG };
    quads.push(Quad::colored([rect.x, rect.y], [rect.w, rect.h], bg));
    let tw = text.measure_width(label, label_size);
    let ascent = text.ascent(label_size);
    let tx = (rect.x + (rect.w - tw) * 0.5).round();
    let ty = (rect.y + (rect.h + ascent) * 0.5).round();
    text.draw(queue, quads, [tx, ty], label, label_size, BTN_LABEL);
}

#[derive(Clone, Copy)]
pub enum TooltipSide {
    Above,
    Below,
}

pub fn draw_tooltip(
    quads: &mut QuadRenderer,
    text: &mut TextRenderer,
    queue: &wgpu::Queue,
    anchor: Rect,
    label: &str,
    size_px: f32,
    side: TooltipSide,
) {
    let tw = text.measure_width(label, size_px);
    let ascent = text.ascent(size_px);
    let box_w = tw + TOOLTIP_PAD_X * 2.0;
    let box_h = ascent + TOOLTIP_PAD_Y * 2.0;
    let bx = (anchor.x + (anchor.w - box_w) * 0.5).round();
    let by = match side {
        TooltipSide::Above => (anchor.y - box_h - TOOLTIP_GAP).round(),
        TooltipSide::Below => (anchor.y + anchor.h + TOOLTIP_GAP).round(),
    };
    quads.push(Quad::colored([bx, by], [box_w, box_h], TOOLTIP_BG));
    text.draw(
        queue,
        quads,
        [bx + TOOLTIP_PAD_X, by + TOOLTIP_PAD_Y + ascent],
        label,
        size_px,
        TOOLTIP_LABEL,
    );
}
