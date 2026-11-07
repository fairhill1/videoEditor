//! Drawing the effects tab of the pool panel, and the label that follows an
//! effect being dragged to a clip.
//!
//! Every rect comes from [`State::effects_panel`], the same layout the press
//! is tested against, so what is drawn is what can be pressed.

use crate::fmt::truncate_to_width;
use crate::input::DragMode;
use crate::effects::EffectKind;
use crate::layout::{EffectsPanel, PoolCell};
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::theme::*;
use crate::ui::{draw_button, draw_slider, draw_tooltip, BtnState, Rect, TooltipSide};

impl State {
    pub(crate) fn draw_effects_panel(&mut self) {
        let panel = self.effects_panel();
        let mut tooltip: Option<(Rect, &'static str)> = None;
        let idle = matches!(self.drag, DragMode::None);

        // Whether the apply buttons have anything to apply to. They take the
        // press either way — the status line then says what is missing — but
        // a button that will do nothing should look it.
        let can_apply = self.selected.iter().any(|id| {
            self.timeline
                .find(*id)
                .and_then(|(t, i)| self.effects_clip_for(t, i))
                .is_some()
        });
        for lib in &panel.library {
            let PoolCell { cell, thumb } = lib.card;
            self.quads.push(Quad::colored([cell.x, cell.y], [cell.w, cell.h], POOL_ROW_COLOR));
            self.quads.push(Quad::colored([thumb.x, thumb.y], [thumb.w, thumb.h], POOL_THUMB_BG));
            draw_effect_art(&mut self.quads, lib.kind, thumb);

            // The name in the band under the picture, as a media tile has.
            let ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
            let baseline = (thumb.y + thumb.h + (POOL_TILE_NAME_H + ascent) * 0.5).round();
            let name = truncate_to_width(&self.text, lib.kind.name(), POOL_ITEM_NAME_SIZE, thumb.w);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [thumb.x, baseline],
                &name,
                POOL_ITEM_NAME_SIZE,
                CLIP_LABEL_COLOR,
            );

            let hovered = idle && lib.apply.contains(self.cursor);
            let state = if can_apply { BtnState::Normal } else { BtnState::Disabled };
            draw_button(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                lib.apply,
                ICON_PLUS,
                TRANSPORT_ICON_SIZE,
                hovered,
                state,
            );
            if hovered {
                tooltip = Some((lib.apply, "Apply to the selected clips"));
            } else if idle && cell.contains(self.cursor) {
                tooltip = Some((cell, lib.kind.blurb()));
            }
        }

        self.draw_clip_effects(&panel, &mut tooltip);

        if let Some((anchor, label)) = tooltip {
            let w = self.logical_size()[0];
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                anchor,
                label,
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                w,
            );
        }
    }

    /// The selected clip's effects: a heading naming the clip, then each
    /// effect with its switch, its name, its remove button and its sliders.
    fn draw_clip_effects(&mut self, panel: &EffectsPanel, tooltip: &mut Option<(Rect, &'static str)>) {
        let heading_w = self.draw_section_heading(panel.clip_heading_y, "SELECTED CLIP");
        let target = self.effects_target();
        let pool_right = self.media_pool_w() - LABEL_PAD;

        let Some((track, idx)) = target else {
            self.draw_hint(panel.hint_y, "Select a video clip to see its effects.");
            return;
        };
        let clip = self.timeline.tracks[track].clips[idx];
        if let Some(src) = self.media.get(clip.source) {
            let x = LABEL_PAD + heading_w + 8.0;
            let name = truncate_to_width(&self.text, &src.name, EFFECTS_SECTION_SIZE, pool_right - x);
            let baseline = self.section_baseline(panel.clip_heading_y);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [x, baseline],
                &name,
                EFFECTS_SECTION_SIZE,
                CLIP_LABEL_COLOR,
            );
        }
        if clip.effects.is_empty() {
            self.draw_hint(panel.hint_y, "No effects yet. Drag one onto the clip, or press +.");
            return;
        }

        let canvas_h = self.canvas().height;
        let idle = matches!(self.drag, DragMode::None);
        for row in &panel.stack {
            let Some(effect) = clip.effects.get(row.index).copied() else {
                continue;
            };
            // One block for the effect and its parameters, so the sliders
            // read as belonging to the name above them.
            let bottom = row.params.last().map_or(row.row.y + row.row.h, |p| p.row.y + p.row.h);
            self.quads.push(Quad::colored(
                [row.row.x, row.row.y],
                [row.row.w, bottom - row.row.y],
                POOL_ROW_COLOR,
            ));

            let toggle_hovered = idle && row.toggle.contains(self.cursor);
            draw_button(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                row.toggle,
                if effect.enabled { ICON_EYE } else { ICON_EYE_OFF },
                TRANSPORT_ICON_SIZE,
                toggle_hovered,
                BtnState::Normal,
            );
            if toggle_hovered {
                *tooltip = Some((row.toggle, if effect.enabled { "Turn off" } else { "Turn on" }));
            }
            let remove_hovered = idle && row.remove.contains(self.cursor);
            draw_button(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                row.remove,
                ICON_CLOSE,
                TRANSPORT_ICON_SIZE,
                remove_hovered,
                BtnState::Normal,
            );
            if remove_hovered {
                *tooltip = Some((row.remove, "Remove effect"));
            }

            let name_x = row.toggle.x + row.toggle.w + 8.0;
            let name_w = (row.remove.x - 8.0 - name_x).max(0.0);
            let name = truncate_to_width(&self.text, effect.kind().name(), EFFECT_NAME_SIZE, name_w);
            let color = if effect.enabled { CLIP_LABEL_COLOR } else { EFFECT_OFF_COLOR };
            let baseline = self.centred_baseline(row.row, EFFECT_NAME_SIZE);
            self.text.draw(&self.queue, &mut self.quads, [name_x, baseline], &name, EFFECT_NAME_SIZE, color);

            for p in &row.params {
                let spec = effect.kind().params()[p.param];
                let value = effect.param(p.param);
                let baseline = self.centred_baseline(p.row, EFFECT_META_SIZE);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [p.row.x + EFFECT_PARAM_INDENT, baseline],
                    spec.name,
                    EFFECT_META_SIZE,
                    EFFECT_META_COLOR,
                );
                let dragging = matches!(
                    self.drag,
                    DragMode::EffectParam { index, param, .. } if index == row.index && param == p.param
                );
                let knob = if dragging {
                    SCROLLBAR_THUMB_ACTIVE_COLOR
                } else if idle && p.row.contains(self.cursor) {
                    SCROLLBAR_THUMB_HOVER_COLOR
                } else {
                    SCROLLBAR_THUMB_COLOR
                };
                draw_slider(&mut self.quads, p.slider, spec.fraction(value), knob);
                let text = spec.format(value, canvas_h);
                let tw = self.text.measure_width(&text, EFFECT_META_SIZE);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [(p.row.x + p.row.w - POOL_ROW_PAD - tw).round(), baseline],
                    &text,
                    EFFECT_META_SIZE,
                    color,
                );
            }
        }
    }

    /// While an effect is being carried to the timeline: its name beside the
    /// cursor, and an outline on the clip it would land on.
    pub(crate) fn draw_effect_drag(&mut self) {
        let DragMode::EffectDrag { kind } = self.drag else {
            return;
        };
        if let Some((track, idx)) = self.effect_drop_target() {
            let layout = self.timeline_layout();
            let clip = self.timeline.tracks[track].clips[idx];
            let x0 = layout.t_to_x(clip.timeline_start).max(layout.clips_x);
            let x1 = layout.t_to_x(clip.timeline_end()).min(layout.clips_right());
            let y = self.lane_top(track, &layout);
            let (w, h, b) = (x1 - x0, layout.lane_h, EFFECT_DROP_OUTLINE_PX);
            if w > 0.0 {
                for (pos, size) in [
                    ([x0, y], [w, b]),
                    ([x0, y + h - b], [w, b]),
                    ([x0, y], [b, h]),
                    ([x1 - b, y], [b, h]),
                ] {
                    self.quads.push(Quad::colored(pos, size, EFFECT_DROP_OUTLINE_COLOR));
                }
            }
        }
        let label = kind.name();
        let tw = self.text.measure_width(label, EFFECT_NAME_SIZE);
        let ascent = self.text.ascent(EFFECT_NAME_SIZE);
        let [cx, cy] = self.cursor;
        let (bx, by) = ((cx + EFFECT_DRAG_OFFSET).round(), (cy + EFFECT_DRAG_OFFSET).round());
        self.quads.push(Quad::colored(
            [bx, by],
            [tw + EFFECT_DRAG_PAD * 2.0, ascent + EFFECT_DRAG_PAD * 2.0],
            EFFECT_DRAG_BG,
        ));
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [bx + EFFECT_DRAG_PAD, by + EFFECT_DRAG_PAD + ascent],
            label,
            EFFECT_NAME_SIZE,
            CLIP_LABEL_COLOR,
        );
    }

    fn section_baseline(&self, top: f32) -> f32 {
        (top + (EFFECTS_SECTION_H + self.text.ascent(EFFECTS_SECTION_SIZE)) * 0.5).round()
    }

    /// A section heading in the band starting at `top`. Returns its width.
    fn draw_section_heading(&mut self, top: f32, label: &str) -> f32 {
        let baseline = self.section_baseline(top);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, baseline],
            label,
            EFFECTS_SECTION_SIZE,
            EFFECTS_SECTION_COLOR,
        );
        self.text.measure_width(label, EFFECTS_SECTION_SIZE)
    }

    fn draw_hint(&mut self, top: f32, hint: &str) {
        let max_w = self.media_pool_w() - LABEL_PAD * 2.0;
        let text = truncate_to_width(&self.text, hint, EFFECT_META_SIZE, max_w);
        let baseline = (top + self.text.ascent(EFFECT_META_SIZE) + 4.0).round();
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, baseline],
            &text,
            EFFECT_META_SIZE,
            EFFECT_META_COLOR,
        );
    }

    fn centred_baseline(&self, row: Rect, size: f32) -> f32 {
        (row.y + (row.h + self.text.ascent(size)) * 0.5).round()
    }
}

/// The picture on an effect's library card: the effect done to a plain shape,
/// drawn from quads so it needs no image files and stays sharp at any size.
fn draw_effect_art(quads: &mut QuadRenderer, kind: EffectKind, slot: Rect) {
    match kind {
        // A crisp square beside the same square blurred: the before and after
        // the effect is for. The blur is faked as rings of falling strength,
        // which is all a blur of a flat shape looks like.
        EffectKind::GaussianBlur => {
            let side = (slot.h * 0.42).min(slot.w * 0.26).round();
            let cy = slot.y + slot.h * 0.5;
            let sharp_x = (slot.x + slot.w * 0.3 - side * 0.5).round();
            quads.push(Quad::colored(
                [sharp_x, (cy - side * 0.5).round()],
                [side, side],
                EFFECT_CARD_ART_COLOR,
            ));
            let soft_cx = slot.x + slot.w * 0.7;
            let spread = side * 0.35;
            let [r, g, b, _] = EFFECT_CARD_ART_COLOR;
            for i in 0..EFFECT_CARD_BLUR_STEPS {
                // Outermost and faintest first, so each ring stacks on the
                // ones outside it and the middle builds up to full strength.
                let f = 1.0 - i as f32 / EFFECT_CARD_BLUR_STEPS as f32;
                let s = side - spread + 2.0 * spread * f;
                let a = 1.6 / EFFECT_CARD_BLUR_STEPS as f32;
                quads.push(Quad::colored(
                    [(soft_cx - s * 0.5).round(), (cy - s * 0.5).round()],
                    [s.round(), s.round()],
                    [r, g, b, a],
                ));
            }
        }
    }
}
