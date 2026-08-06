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

const BTN_BG: [f32; 4] = [0.22, 0.22, 0.27, 0.94];
const BTN_BG_HOVER: [f32; 4] = [0.33, 0.33, 0.40, 0.96];
const BTN_BG_DISABLED: [f32; 4] = [0.16, 0.16, 0.19, 0.92];
// A single 1px lit top edge, with no matching shadow beneath. That asymmetry is
// the point: a full bright-top/dark-bottom bevel reads as dated, while a lone
// top highlight still says "raised" because light falls from above.
const BTN_TOP_HIGHLIGHT: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
const BTN_LABEL: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const BTN_LABEL_DISABLED: [f32; 4] = [0.42, 0.42, 0.47, 1.0];
// Indicator strip down the left edge of a toggle. The off color is dim but
// clearly present: a strip that vanished entirely would lose the cue that the
// button is a toggle at all, leaving "off" indistinguishable from a plain
// button.
const TOGGLE_ON: [f32; 4] = [0.95, 0.55, 0.15, 1.0];
const TOGGLE_OFF: [f32; 4] = [0.30, 0.30, 0.34, 1.0];
const TOGGLE_STRIP_W: f32 = 3.0;
// Kept darker than any panel so it reads as a chip floating over the UI. The
// border is what stops it looking like a hole punched into the panel now that
// surfaces sit well above it in value.
const TOOLTIP_BG: [f32; 4] = [0.06, 0.06, 0.08, 1.0];
const TOOLTIP_BORDER: [f32; 4] = [0.34, 0.34, 0.40, 1.0];
const TOOLTIP_LABEL: [f32; 4] = [0.96, 0.96, 0.98, 1.0];
const TOOLTIP_GAP: f32 = 7.0;
const TOOLTIP_PAD_X: f32 = 9.0;
const TOOLTIP_PAD_Y: f32 = 5.0;
// Flat quads can't blur, so the shadow is faked with a few concentric rects,
// each a pixel wider and all at low alpha. Three steps is enough to read as a
// soft edge; more starts looking like a halo, which is louder than a tooltip
// should ever be.
const TOOLTIP_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.10];
const TOOLTIP_SHADOW_STEPS: i32 = 3;

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

    // Skipped when disabled — an unavailable control shouldn't look raised.
    if !disabled {
        quads.push(Quad::colored(
            [rect.x, rect.y],
            [rect.w, 1.0],
            BTN_TOP_HIGHLIGHT,
        ));
    }

    // After the highlight, so the indicator stays a clean unbroken bar.
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
    // Shadow is nudged a pixel down so the light reads as coming from above,
    // matching the buttons' top highlight.
    for step in (1..=TOOLTIP_SHADOW_STEPS).rev() {
        let g = step as f32;
        quads.push(Quad::colored(
            [bx - g, by - g + 1.0],
            [box_w + g * 2.0, box_h + g * 2.0],
            TOOLTIP_SHADOW,
        ));
    }
    quads.push(Quad::colored(
        [bx - 1.0, by - 1.0],
        [box_w + 2.0, box_h + 2.0],
        TOOLTIP_BORDER,
    ));
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
