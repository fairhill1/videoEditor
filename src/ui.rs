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
const BTN_BG_DISABLED: [f32; 4] = [0.13, 0.13, 0.16, 0.92];
const BTN_LABEL: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const BTN_LABEL_DISABLED: [f32; 4] = [0.42, 0.42, 0.47, 1.0];
// Indicator strip down the left edge of a toggle. The off color is dim but
// clearly present: a strip that vanished entirely would lose the cue that the
// button is a toggle at all, leaving "off" indistinguishable from a plain
// button.
const TOGGLE_ON: [f32; 4] = [0.95, 0.55, 0.15, 1.0];
const TOGGLE_OFF: [f32; 4] = [0.30, 0.30, 0.34, 1.0];
const TOGGLE_STRIP_W: f32 = 3.0;
const TOOLTIP_BG: [f32; 4] = [0.05, 0.05, 0.07, 1.0];
const TOOLTIP_LABEL: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const TOOLTIP_GAP: f32 = 6.0;
const TOOLTIP_PAD_X: f32 = 7.0;
const TOOLTIP_PAD_Y: f32 = 4.0;

/// What a button says about itself beyond hover.
///
/// `Disabled` and `Toggle(false)` are deliberately distinct: greying out means
/// "you can't press this", while a toggle that is off is fully pressable and
/// must not look broken. They get different treatments — dimming versus an
/// unlit indicator.
#[derive(Clone, Copy, PartialEq)]
pub enum BtnState {
    Normal,
    /// Action unavailable right now, e.g. undo with an empty history.
    Disabled,
    /// Persistent on/off setting, shown by the indicator strip.
    Toggle(bool),
}

pub fn draw_button(
    quads: &mut QuadRenderer,
    text: &mut TextRenderer,
    queue: &wgpu::Queue,
    rect: Rect,
    label: &str,
    label_size: f32,
    hovered: bool,
    state: BtnState,
) {
    let disabled = state == BtnState::Disabled;
    let bg = match (disabled, hovered) {
        (true, _) => BTN_BG_DISABLED,
        (false, true) => BTN_BG_HOVER,
        (false, false) => BTN_BG,
    };
    let fg = if disabled { BTN_LABEL_DISABLED } else { BTN_LABEL };
    quads.push(Quad::colored([rect.x, rect.y], [rect.w, rect.h], bg));

    if let BtnState::Toggle(on) = state {
        let strip = if on { TOGGLE_ON } else { TOGGLE_OFF };
        quads.push(Quad::colored(
            [rect.x, rect.y],
            [TOGGLE_STRIP_W, rect.h],
            strip,
        ));
    }

    // Centered on the whole rect, strip included, so a toggle's label keeps
    // the same rhythm as the plain buttons beside it.
    let tw = text.measure_width(label, label_size);
    let ascent = text.ascent(label_size);
    let tx = (rect.x + (rect.w - tw) * 0.5).round();
    let ty = (rect.y + (rect.h + ascent) * 0.5).round();
    text.draw(queue, quads, [tx, ty], label, label_size, fg);
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
