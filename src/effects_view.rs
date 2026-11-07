//! Drawing the effects tab of the pool panel, and the label that follows an
//! effect being dragged to a clip.
//!
//! Every rect comes from [`State::effects_panel`], the same layout the press
//! is tested against, so what is drawn is what can be pressed.

use crate::fmt::truncate_to_width;
use crate::fonts;
use crate::input::DragMode;
use crate::effects::EffectKind;
use crate::layout::{
    color_chip_rect, swatch_rects, transform_fraction, EffectsPanel, PoolCell, TitleControl, TitleMenu,
    TitleRow,
};
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::textedit::Cursor;
use crate::theme::*;
use crate::timeline::Clip;
use crate::title::{self, Title, TitleColor, TitleNumber, TitlePart, SWATCHES};
use crate::ui::{
    draw_button, draw_menu_row, draw_slider, draw_tooltip, BtnState, Rect, TooltipSide,
};

impl State {
    pub(crate) fn draw_effects_panel(&mut self) {
        let panel = self.effects_panel();
        let mut tooltip: Option<(Rect, &'static str)> = None;
        // Hover means something only over the part of the panel in view, and
        // not while the font list is open over it.
        let [top, bottom] = panel.visible;
        let idle = matches!(self.drag, DragMode::None)
            && self.font_menu.is_none()
            && self.color_picker.is_none()
            && (top..bottom).contains(&self.cursor[1]);
        self.quads.set_clip_y(top, bottom);

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

        self.draw_clip_effects(&panel, idle, &mut tooltip);
        self.quads.clear_clip_y();

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
    fn draw_clip_effects(
        &mut self,
        panel: &EffectsPanel,
        idle: bool,
        tooltip: &mut Option<(Rect, &'static str)>,
    ) {
        let heading_w = self.draw_section_heading(panel.clip_heading_y, "SELECTED CLIP");
        let target = self.effects_target();
        let pool_right = self.media_pool_w() - LABEL_PAD;

        let Some((track, idx)) = target else {
            self.draw_hint(panel.hint_y, "Select a video clip to see its settings.");
            return;
        };
        let clip = self.timeline.tracks[track].clips[idx];
        if let Some(src) = self.media.get(clip.source) {
            let x = LABEL_PAD + heading_w + 8.0;
            // Several titles selected are styled together, and several
            // clips placed together; the heading says so, since the panel
            // shows only the first of them.
            let titles = self.styled_titles().len().saturating_sub(1);
            let clips = self.selected_pictures().len().saturating_sub(1);
            let label = match (titles, clips) {
                (0, 0) => src.name.clone(),
                (1, _) => format!("{} + 1 more title", src.name),
                (n, _) if n > 1 => format!("{} + {n} more titles", src.name),
                (_, 1) => format!("{} + 1 more clip", src.name),
                (_, n) => format!("{} + {n} more clips", src.name),
            };
            let name = truncate_to_width(&self.text, &label, EFFECTS_SECTION_SIZE, pool_right - x);
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
        if let Some((_, title)) = self.effects_title() {
            let title = title.clone();
            self.draw_title_settings(panel, &title, idle, tooltip);
        }
        self.draw_transform(panel, &clip, idle, tooltip);
        if clip.effects.is_empty() {
            self.draw_hint(panel.hint_y, "No effects yet. Drag one onto the clip, or press +.");
            return;
        }

        let canvas_h = self.canvas().height;
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

    /// Where the clip sits on the canvas: its zoom and its position, each a
    /// slider, under a heading with a button that puts it back.
    fn draw_transform(
        &mut self,
        panel: &EffectsPanel,
        clip: &Clip,
        idle: bool,
        tooltip: &mut Option<(Rect, &'static str)>,
    ) {
        let Some(t) = &panel.transform else {
            return;
        };
        let g = t.group;
        self.quads.push(Quad::colored([g.x, g.y], [g.w, g.h], POOL_ROW_COLOR));
        let baseline = self.centred_baseline(t.heading, EFFECT_NAME_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [t.heading.x + EFFECT_PARAM_INDENT, baseline],
            "Transform",
            EFFECT_NAME_SIZE,
            CLIP_LABEL_COLOR,
        );
        let placed = clip.transform != crate::timeline::Transform::default();
        let reset_hovered = idle && t.reset.contains(self.cursor);
        draw_button(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            t.reset,
            ICON_RESET,
            TRANSPORT_ICON_SIZE,
            reset_hovered,
            if placed { BtnState::Normal } else { BtnState::Disabled },
        );
        if reset_hovered {
            *tooltip = Some((t.reset, "Reset: centred, at 100%"));
        }

        let reach = self.transform_reach(clip);
        for r in &t.rows {
            let value = r.field.get(&clip.transform);
            let baseline = self.centred_baseline(r.row, EFFECT_META_SIZE);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [r.row.x + EFFECT_PARAM_INDENT, baseline],
                r.field.name(),
                EFFECT_META_SIZE,
                EFFECT_META_COLOR,
            );
            let dragging = matches!(self.drag, DragMode::Transform { field, .. } if field == r.field);
            let knob = if dragging {
                SCROLLBAR_THUMB_ACTIVE_COLOR
            } else if idle && r.row.contains(self.cursor) {
                SCROLLBAR_THUMB_HOVER_COLOR
            } else {
                SCROLLBAR_THUMB_COLOR
            };
            draw_slider(&mut self.quads, r.slider, transform_fraction(r.field, value, reach), knob);
            let text = self.format_transform(r.field, value);
            let tw = self.text.measure_width(&text, EFFECT_META_SIZE);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [(r.row.x + r.row.w - POOL_ROW_PAD - tw).round(), baseline],
                &text,
                EFFECT_META_SIZE,
                CLIP_LABEL_COLOR,
            );
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

    /// A title's settings: the words, the face, the size and colour, then
    /// the outline and the shadow, each a block with its switch at the top.
    fn draw_title_settings(
        &mut self,
        panel: &EffectsPanel,
        title: &Title,
        idle: bool,
        tooltip: &mut Option<(Rect, &'static str)>,
    ) {
        for g in &panel.title_groups {
            self.quads.push(Quad::colored([g.x, g.y], [g.w, g.h], POOL_ROW_COLOR));
        }
        let canvas_h = self.canvas().height;
        // This title, not just any: one being typed into from elsewhere
        // leaves this box as it is.
        let shown = self.effects_title().map(|(source, _)| source);
        let typing = shown.is_some() && self.editing_title.as_ref().map(|(id, _)| *id) == shown;
        for &TitleRow { control, row, field } in &panel.title {
            // Rows under a switched-off outline or shadow dim with it, and
            // still work: setting one up before turning it on is fine.
            let on = match control {
                TitleControl::Number(TitleNumber::OutlineWidth)
                | TitleControl::Color(TitleColor::Outline) => title.outline.enabled,
                TitleControl::Number(
                    TitleNumber::ShadowDistance
                    | TitleNumber::ShadowSoftness
                    | TitleNumber::ShadowOpacity,
                ) => title.shadow.enabled,
                _ => true,
            };
            let color = if on { CLIP_LABEL_COLOR } else { EFFECT_OFF_COLOR };
            let label = match control {
                TitleControl::Text => Some("Text"),
                TitleControl::Font => Some("Font"),
                TitleControl::Weight => Some("Weight"),
                TitleControl::Number(n) => Some(n.spec().name),
                TitleControl::Color(_) => Some("Colour"),
                TitleControl::Toggle(_) => None,
            };
            if let Some(label) = label {
                // A colour row's label sits in its top half, over the chip.
                let band = match control {
                    TitleControl::Color(_) => Rect { h: row.h * 0.5, ..row },
                    _ => row,
                };
                let baseline = self.centred_baseline(band, EFFECT_META_SIZE);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [row.x + EFFECT_PARAM_INDENT, baseline],
                    label,
                    EFFECT_META_SIZE,
                    EFFECT_META_COLOR,
                );
            }
            let hovered = idle && row.contains(self.cursor);
            match control {
                TitleControl::Text => {
                    let field_hovered = idle && field.contains(self.cursor);
                    let cursor = typing.then_some(self.title_cursor);
                    self.draw_text_box(field, &title.text, field_hovered, cursor);
                    if field_hovered && !typing {
                        *tooltip = Some((field, "Click to type"));
                    }
                }
                TitleControl::Weight => {
                    let weight = fonts::shown_weight(title.font.as_deref(), title.weight);
                    let name = fonts::weight_name(weight);
                    let open = matches!(self.font_menu, Some((TitleMenu::Weight, _)));
                    let field_hovered = idle && field.contains(self.cursor);
                    self.draw_field(field, &name, field_hovered, open, Some(ICON_CHEVRON_DOWN));
                }
                TitleControl::Font => {
                    let name = title.font.as_deref().unwrap_or("Default");
                    let open = matches!(self.font_menu, Some((TitleMenu::Font, _)));
                    let field_hovered = idle && field.contains(self.cursor);
                    self.draw_field(field, name, field_hovered, open, Some(ICON_CHEVRON_DOWN));
                }
                TitleControl::Number(n) => {
                    let spec = n.spec();
                    let value = n.get(title);
                    let dragging = matches!(
                        self.drag,
                        DragMode::TitleNumber { field, .. } if field == n
                    );
                    let knob = if dragging {
                        SCROLLBAR_THUMB_ACTIVE_COLOR
                    } else if hovered {
                        SCROLLBAR_THUMB_HOVER_COLOR
                    } else {
                        SCROLLBAR_THUMB_COLOR
                    };
                    draw_slider(&mut self.quads, field, spec.fraction(value), knob);
                    let text = spec.format(value, canvas_h);
                    let tw = self.text.measure_width(&text, EFFECT_META_SIZE);
                    let baseline = self.centred_baseline(row, EFFECT_META_SIZE);
                    self.text.draw(
                        &self.queue,
                        &mut self.quads,
                        [(row.x + row.w - POOL_ROW_PAD - tw).round(), baseline],
                        &text,
                        EFFECT_META_SIZE,
                        color,
                    );
                }
                TitleControl::Color(c) => {
                    let current = c.get(title);
                    let chip = color_chip_rect(row);
                    let open = self.color_picker.as_ref().is_some_and(|p| p.field == c);
                    let chip_hovered = idle && chip.contains(self.cursor);
                    self.draw_color_chip(chip, current, chip_hovered, open);
                    if chip_hovered {
                        *tooltip = Some((chip, "Pick any colour"));
                    }
                    for (swatch, rect) in SWATCHES.iter().zip(swatch_rects(field)) {
                        let chosen = swatch[..3]
                            .iter()
                            .zip(&current[..3])
                            .all(|(a, b)| (a - b).abs() < 0.01);
                        let ring = if chosen {
                            Some(TITLE_SWATCH_RING)
                        } else if idle && rect.contains(self.cursor) {
                            Some(TITLE_FIELD_HOVER_BORDER)
                        } else {
                            None
                        };
                        if let Some(ring) = ring {
                            let g = TITLE_SWATCH_RING_PX;
                            self.quads.push(Quad::colored(
                                [rect.x - g, rect.y - g],
                                [rect.w + g * 2.0, rect.h + g * 2.0],
                                ring,
                            ));
                        }
                        // A hairline edge, so black reads against the dark row.
                        self.quads.push(Quad::colored([rect.x, rect.y], [rect.w, rect.h], TITLE_SWATCH_EDGE));
                        let mut fill = *swatch;
                        if !on {
                            fill[3] = 0.4;
                        }
                        self.quads.push(Quad::colored(
                            [rect.x + 1.0, rect.y + 1.0],
                            [rect.w - 2.0, rect.h - 2.0],
                            fill,
                        ));
                    }
                }
                TitleControl::Toggle(part) => {
                    let enabled = part.enabled(title);
                    draw_button(
                        &mut self.quads,
                        &mut self.text,
                        &self.queue,
                        field,
                        if enabled { ICON_EYE } else { ICON_EYE_OFF },
                        TRANSPORT_ICON_SIZE,
                        hovered,
                        BtnState::Normal,
                    );
                    if hovered {
                        *tooltip = Some((field, match (part, enabled) {
                            (TitlePart::Outline, true) => "Turn the outline off",
                            (TitlePart::Outline, false) => "Turn the outline on",
                            (TitlePart::Shadow, true) => "Turn the shadow off",
                            (TitlePart::Shadow, false) => "Turn the shadow on",
                        }));
                    }
                    let name_x = field.x + field.w + 8.0;
                    let baseline = self.centred_baseline(row, EFFECT_NAME_SIZE);
                    let color = if enabled { CLIP_LABEL_COLOR } else { EFFECT_OFF_COLOR };
                    self.text.draw(
                        &self.queue,
                        &mut self.quads,
                        [name_x, baseline],
                        part.name(),
                        EFFECT_NAME_SIZE,
                        color,
                    );
                }
            }
        }
    }

    /// Which of a title's lines the text box shows, and from where along
    /// each: all of them from their start, unless the caret (`Some` while
    /// typing) would be out of sight — then the lines and the letters scroll
    /// to keep it in. The one answer drawing and a click on the box share.
    pub(crate) fn text_box_view(&self, r: Rect, text: &str, caret: Option<usize>) -> TextBoxView {
        let lines: Vec<&str> = text.split('\n').collect();
        let caret_at = caret.map(|i| title::caret::line_col(text, i.min(text.len())).0);
        let skip = match caret_at {
            Some(line) if line >= TITLE_TEXT_MAX_LINES => line + 1 - TITLE_TEXT_MAX_LINES,
            _ => 0,
        };
        let room = (r.w - TITLE_FIELD_PAD * 2.0 - TITLE_CARET_W - 1.0).max(0.0);
        let mut start = 0;
        let mut rows = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let line_start = start;
            start += line.len() + 1;
            if i < skip || i >= skip + TITLE_TEXT_MAX_LINES {
                continue;
            }
            // On the caret's line, letters off the start until the caret is
            // in the box.
            let mut from = 0;
            if caret_at == Some(i) {
                let to = caret.unwrap_or(0).min(text.len()) - line_start;
                while from < to && self.text.measure_width(&line[from..to], TITLE_FIELD_SIZE) > room {
                    from = title::caret::right(line, from);
                }
            }
            rows.push(TextBoxRow { start: line_start, from: line_start + from, end: line_start + line.len() });
        }
        TextBoxView { rows }
    }

    /// The position in `text` nearest to `p`, for a click on its box.
    pub(crate) fn text_box_hit(&self, r: Rect, text: &str, caret: Option<usize>, p: [f32; 2]) -> usize {
        let view = self.text_box_view(r, text, caret);
        let first_top = r.y + (TITLE_FIELD_H - TITLE_TEXT_LINE_H) * 0.5;
        let row = (((p[1] - first_top) / TITLE_TEXT_LINE_H).floor().max(0.0) as usize)
            .min(view.rows.len().saturating_sub(1));
        let Some(row) = view.rows.get(row) else {
            return text.len();
        };
        let x = p[0] - (r.x + TITLE_FIELD_PAD);
        let shown = &text[row.from..row.end];
        let mut best = row.from;
        let mut best_d = x.abs();
        for (j, ch) in shown.char_indices() {
            let after = j + ch.len_utf8();
            let d = (self.text.measure_width(&shown[..after], TITLE_FIELD_SIZE) - x).abs();
            if d < best_d {
                best_d = d;
                best = row.from + after;
            }
        }
        best
    }

    /// The title's words, every line of them, in a box that grows to hold
    /// them, with the caret where it is while being typed into.
    fn draw_text_box(&mut self, r: Rect, text: &str, hovered: bool, cursor: Option<Cursor>) {
        self.draw_field(r, "", hovered, cursor.is_some(), None);
        let caret = cursor.map(|c| c.caret);
        let selection = cursor.and_then(|c| c.range());
        let view = self.text_box_view(r, text, caret);
        let max_w = (r.w - TITLE_FIELD_PAD * 2.0).max(0.0);
        let ascent = self.text.ascent(TITLE_FIELD_SIZE);
        let first_baseline =
            r.y + (TITLE_FIELD_H - TITLE_TEXT_LINE_H) * 0.5 + (TITLE_TEXT_LINE_H + ascent) * 0.5;
        for (n, row) in view.rows.iter().enumerate() {
            let baseline = (first_baseline + n as f32 * TITLE_TEXT_LINE_H).round();
            let x0 = r.x + TITLE_FIELD_PAD;
            // The selection behind the letters: the part of it on this line,
            // and a sliver past the end where it runs on over the line break.
            if let Some((a, b)) = selection {
                let lo = a.max(row.from);
                let hi = b.min(row.end);
                if lo <= hi && (lo < hi || b > row.end) && a <= row.end && b >= row.from {
                    let x_at = |i: usize| x0 + self.text.measure_width(&text[row.from..i], TITLE_FIELD_SIZE);
                    let x1 = (x_at(lo)).min(r.x + r.w - TITLE_FIELD_PAD);
                    let mut x2 = (x_at(hi)).min(r.x + r.w - TITLE_FIELD_PAD);
                    if b > row.end {
                        x2 = (x2 + TITLE_SELECTION_EOL_W).min(r.x + r.w - 1.0);
                    }
                    self.quads.push(Quad::colored(
                        [x1.round(), (baseline - ascent).round() - 1.0],
                        [(x2 - x1).round().max(1.0), (ascent * 1.25).round() + 2.0],
                        TEXT_SELECTION_COLOR,
                    ));
                }
            }
            let shown = truncate_to_width(&self.text, &text[row.from..row.end], TITLE_FIELD_SIZE, max_w);
            self.text.draw(&self.queue, &mut self.quads, [x0, baseline], &shown, TITLE_FIELD_SIZE, CLIP_LABEL_COLOR);
            if let Some(i) = caret.filter(|&i| i >= row.start && i <= row.end) {
                let x = (x0 + self.text.measure_width(&text[row.from..i.max(row.from)], TITLE_FIELD_SIZE)).round();
                self.quads.push(Quad::colored(
                    [x, (baseline - ascent).round()],
                    [TITLE_CARET_W, (ascent * 1.25).round()],
                    TITLE_CARET_COLOR,
                ));
            }
        }
    }

    /// The place among the hex box's digits nearest to `p`.
    pub(crate) fn hex_box_hit(&self, r: Rect, p: [f32; 2]) -> usize {
        let digits = self.color_picker.as_ref().and_then(|c| c.hex.clone()).unwrap_or_default();
        let shown = format!("#{digits}");
        let x = p[0] - (r.x + TITLE_FIELD_PAD);
        (0..=digits.len())
            .min_by(|&a, &b| {
                let d = |i: usize| (self.text.measure_width(&shown[..1 + i], TITLE_FIELD_SIZE) - x).abs();
                d(a).total_cmp(&d(b))
            })
            .unwrap_or(0)
    }

    /// A box with words in it: the title's text, or its font with a chevron
    /// saying it opens a list. `active` while it is being typed into or its
    /// list is open.
    fn draw_field(&mut self, r: Rect, words: &str, hovered: bool, active: bool, icon: Option<char>) {
        let border = if active {
            TITLE_FIELD_ACTIVE_BORDER
        } else if hovered {
            TITLE_FIELD_HOVER_BORDER
        } else {
            TITLE_FIELD_BORDER
        };
        self.quads.push(Quad::colored([r.x, r.y], [r.w, r.h], border));
        self.quads.push(Quad::colored([r.x + 1.0, r.y + 1.0], [r.w - 2.0, r.h - 2.0], TITLE_FIELD_BG));
        let icon_w = icon.map_or(0.0, |_| r.h);
        if let Some(icon) = icon {
            let glyph = icon.to_string();
            let gw = self.text.measure_width(&glyph, TITLE_FIELD_SIZE);
            let baseline = self.centred_baseline(r, TITLE_FIELD_SIZE);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [(r.x + r.w - (icon_w + gw) * 0.5).round(), baseline],
                &glyph,
                TITLE_FIELD_SIZE,
                EFFECT_META_COLOR,
            );
        }
        let max_w = (r.w - TITLE_FIELD_PAD * 2.0 - icon_w).max(0.0);
        if words.is_empty() {
            return;
        }
        let shown = truncate_to_width(&self.text, words, TITLE_FIELD_SIZE, max_w);
        let baseline = self.centred_baseline(r, TITLE_FIELD_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [r.x + TITLE_FIELD_PAD, baseline],
            &shown,
            TITLE_FIELD_SIZE,
            CLIP_LABEL_COLOR,
        );
    }

    /// The list of installed fonts, dropped from the font box: over
    /// everything, like the project popup.
    pub(crate) fn draw_font_menu(&mut self) {
        let Some((kind, menu)) = State::font_menu(self) else {
            return;
        };
        let (current, weight) = self
            .effects_title()
            .map(|(_, t)| (t.font.clone(), fonts::shown_weight(t.font.as_deref(), t.weight)))
            .unwrap_or((None, fonts::REGULAR));
        let weights = self.shown_weights();
        let Rect { x, y, w, h } = menu.rect;
        for step in (1..=MENU_SHADOW_STEPS).rev() {
            let g = step as f32;
            self.quads.push(Quad::colored([x - g, y - g + 1.0], [w + g * 2.0, h + g * 2.0], MENU_SHADOW));
        }
        self.quads.push(Quad::colored([x - 1.0, y - 1.0], [w + 2.0, h + 2.0], MENU_BORDER));
        self.quads.push(Quad::colored([x, y], [w, h], MENU_BG));
        for &(i, rect) in &menu.rows {
            let (label, chosen) = match kind {
                TitleMenu::Font => {
                    let family = fonts::choice(i);
                    (family.unwrap_or("Default").to_string(), family.map(str::to_string) == current)
                }
                TitleMenu::Weight => {
                    let w = weights.get(i).copied().unwrap_or(fonts::REGULAR);
                    (fonts::weight_name(w), w == weight)
                }
            };
            draw_menu_row(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                rect,
                &label,
                MENU_ROW_SIZE,
                rect.contains(self.cursor),
                chosen,
            );
        }
        // Where in the whole list the rows shown are, along the right edge.
        let n = match kind {
            TitleMenu::Font => fonts::choice_count(),
            TitleMenu::Weight => weights.len(),
        };
        if menu.max_scroll > 0 {
            let first = menu.rows.first().map_or(0, |r| r.0);
            let track_h = h - MENU_PAD * 2.0;
            let thumb_h = (track_h * menu.rows.len() as f32 / n as f32).max(MENU_ROW_H * 0.5);
            let thumb_y = y + MENU_PAD + (track_h - thumb_h) * first as f32 / menu.max_scroll as f32;
            self.quads.push(Quad::colored(
                [x + w - MENU_PAD * 0.5 - 1.5, thumb_y.round()],
                [3.0, thumb_h.round()],
                SCROLLBAR_THUMB_COLOR,
            ));
        }
    }

    /// A colour shown as a chip: a framed square of it. `active` when it is
    /// what the picker is open on.
    fn draw_color_chip(&mut self, r: Rect, color: [f32; 4], hovered: bool, active: bool) {
        let border = if active {
            TITLE_FIELD_ACTIVE_BORDER
        } else if hovered {
            TITLE_FIELD_HOVER_BORDER
        } else {
            TITLE_FIELD_BORDER
        };
        self.quads.push(Quad::colored([r.x, r.y], [r.w, r.h], border));
        self.quads.push(Quad::colored([r.x + 1.0, r.y + 1.0], [r.w - 2.0, r.h - 2.0], TITLE_SWATCH_EDGE));
        self.quads.push(Quad::colored(
            [r.x + 2.0, r.y + 2.0],
            [r.w - 4.0, r.h - 4.0],
            [color[0], color[1], color[2], 1.0],
        ));
    }

    /// The colour picker, over everything, like the other popups.
    pub(crate) fn draw_color_picker(&mut self) {
        let Some(layout) = self.picker_layout() else {
            return;
        };
        let Some(current) = self.picker_color() else {
            return;
        };
        let Some(picker) = self.color_picker.as_ref() else {
            return;
        };
        let [hue, s, v] = picker.hsv;
        let hex = picker.hex.clone();
        let hex_cursor = Some(picker.hex_cursor);
        let armed = picker.eyedropper;
        let idle = matches!(self.drag, DragMode::None) && !armed;
        let cursor = self.cursor;

        let Rect { x, y, w, h } = layout.rect;
        for step in (1..=MENU_SHADOW_STEPS).rev() {
            let g = step as f32;
            self.quads.push(Quad::colored([x - g, y - g + 1.0], [w + g * 2.0, h + g * 2.0], MENU_SHADOW));
        }
        self.quads.push(Quad::colored([x - 1.0, y - 1.0], [w + 2.0, h + 2.0], MENU_BORDER));
        self.quads.push(Quad::colored([x, y], [w, h], MENU_BG));

        // The two gradients, each in a hairline frame, sampled from texel
        // centre to texel centre so the ends are the colours they claim.
        for r in [layout.sv, layout.hue] {
            self.quads.push(Quad::colored([r.x - 1.0, r.y - 1.0], [r.w + 2.0, r.h + 2.0], TITLE_FIELD_BORDER));
        }
        let (sv_rect, hue_rect) = (layout.sv, layout.hue);
        if self.picker_textures().is_some() {
            let inset = |n: u32| 0.5 / n as f32;
            let (a, b) = (inset(crate::picker::SV_TEXELS), inset(crate::picker::HUE_TEXELS));
            let sv_quad = Quad {
                uv: [a, a, 1.0 - a, 1.0 - a],
                ..Quad::textured([sv_rect.x, sv_rect.y], [sv_rect.w, sv_rect.h])
            };
            let hue_quad = Quad {
                uv: [0.0, b, 1.0, 1.0 - b],
                ..Quad::textured([hue_rect.x, hue_rect.y], [hue_rect.w, hue_rect.h])
            };
            if let Some(tex) = &self.picker_tex {
                self.quads.push_with(sv_quad, Some(&tex.sv));
                self.quads.push_with(hue_quad, Some(&tex.hue));
            }
        }

        // Where the colour is on the square: a ring of white in a ring of
        // black, which reads on any colour, around the colour itself.
        let mx = (sv_rect.x + s * sv_rect.w).round();
        let my = (sv_rect.y + (1.0 - v) * sv_rect.h).round();
        let m = PICKER_MARK;
        self.quads.push(Quad::colored([mx - m * 0.5 - 1.0, my - m * 0.5 - 1.0], [m + 2.0, m + 2.0], PICKER_MARK_EDGE));
        self.quads.push(Quad::colored([mx - m * 0.5, my - m * 0.5], [m, m], PICKER_MARK_COLOR));
        self.quads.push(Quad::colored(
            [mx - m * 0.5 + 2.0, my - m * 0.5 + 2.0],
            [m - 4.0, m - 4.0],
            [current[0], current[1], current[2], 1.0],
        ));
        let hy = (hue_rect.y + hue * hue_rect.h).round();
        self.quads.push(Quad::colored([hue_rect.x - 2.0, hy - 2.5], [hue_rect.w + 4.0, 5.0], PICKER_MARK_EDGE));
        self.quads.push(Quad::colored([hue_rect.x - 1.0, hy - 1.5], [hue_rect.w + 2.0, 3.0], PICKER_MARK_COLOR));

        // The colour as it is, its hex code, and the eyedropper.
        self.draw_color_chip(layout.current, current, false, false);
        let typing = hex.is_some();
        let digits = hex.unwrap_or_else(|| crate::color::to_hex(current));
        let shown = format!("#{digits}");
        let hex_hovered = idle && layout.hex.contains(cursor);
        let pad = (layout.hex.h - TITLE_TEXT_LINE_H) * 0.5;
        // Where position `i` among the digits sits, after the `#`.
        let x_at = |s: &Self, i: usize| {
            layout.hex.x + TITLE_FIELD_PAD + s.text.measure_width(&shown[..1 + i], TITLE_FIELD_SIZE)
        };
        self.draw_field(layout.hex, "", hex_hovered, typing, None);
        if let Some((a, b)) = hex_cursor.filter(|_| typing).and_then(|c| c.range()) {
            let (x1, x2) = (x_at(self, a), x_at(self, b));
            self.quads.push(Quad::colored(
                [x1.round(), layout.hex.y + pad],
                [(x2 - x1).round().max(1.0), TITLE_TEXT_LINE_H],
                TEXT_SELECTION_COLOR,
            ));
        }
        let baseline = self.centred_baseline(layout.hex, TITLE_FIELD_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [layout.hex.x + TITLE_FIELD_PAD, baseline],
            &shown,
            TITLE_FIELD_SIZE,
            CLIP_LABEL_COLOR,
        );
        if let Some(c) = hex_cursor.filter(|_| typing) {
            let caret_x = x_at(self, c.caret.min(digits.len())).round();
            self.quads.push(Quad::colored(
                [caret_x, layout.hex.y + pad],
                [TITLE_CARET_W, TITLE_TEXT_LINE_H],
                TITLE_CARET_COLOR,
            ));
        }
        let dropper_hovered = layout.eyedropper.contains(cursor) && matches!(self.drag, DragMode::None);
        let dropper_state = if self.surface_copy_src { BtnState::Toggle(armed) } else { BtnState::Disabled };
        draw_button(
            &mut self.quads,
            &mut self.text,
            &self.queue,
            layout.eyedropper,
            ICON_PIPETTE,
            TRANSPORT_ICON_SIZE,
            dropper_hovered,
            dropper_state,
        );

        for (i, r) in layout.swatches.iter().enumerate() {
            let fill = SWATCHES[i];
            let hovered = idle && r.contains(cursor);
            self.draw_swatch(*r, fill, crate::color::same(fill, current), hovered);
        }
        for (i, r) in layout.recent.iter().enumerate() {
            match self.recent_colors.get(i).copied() {
                Some(fill) => {
                    let hovered = idle && r.contains(cursor);
                    self.draw_swatch(*r, fill, false, hovered);
                }
                None => self.quads.push(Quad::colored([r.x, r.y], [r.w, r.h], PICKER_EMPTY_SLOT)),
            }
        }

        let tooltip = if dropper_hovered {
            Some((
                layout.eyedropper,
                if !self.surface_copy_src {
                    "Picking from the screen isn't supported here"
                } else if armed {
                    "Click anywhere to take its colour"
                } else {
                    "Pick a colour from the screen"
                },
            ))
        } else if hex_hovered && !typing {
            Some((layout.hex, "Click to type a hex code"))
        } else if idle && layout.recent.iter().take(self.recent_colors.len()).any(|r| r.contains(cursor)) {
            Some((layout.rect, "Recent colours"))
        } else {
            None
        };
        if let Some((anchor, label)) = tooltip {
            let [win_w, _] = self.logical_size();
            draw_tooltip(
                &mut self.quads,
                &mut self.text,
                &self.queue,
                anchor,
                label,
                TRANSPORT_TOOLTIP_SIZE,
                TooltipSide::Below,
                win_w,
            );
        }
    }

    /// One colour swatch: a hairline edge so black reads on the dark
    /// panel, and a ring when it is the colour in use or under the cursor.
    fn draw_swatch(&mut self, r: Rect, fill: [f32; 4], chosen: bool, hovered: bool) {
        let ring = if chosen {
            Some(TITLE_SWATCH_RING)
        } else if hovered {
            Some(TITLE_FIELD_HOVER_BORDER)
        } else {
            None
        };
        if let Some(ring) = ring {
            let g = TITLE_SWATCH_RING_PX;
            self.quads.push(Quad::colored([r.x - g, r.y - g], [r.w + g * 2.0, r.h + g * 2.0], ring));
        }
        self.quads.push(Quad::colored([r.x, r.y], [r.w, r.h], TITLE_SWATCH_EDGE));
        self.quads.push(Quad::colored([r.x + 1.0, r.y + 1.0], [r.w - 2.0, r.h - 2.0], fill));
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

/// What the text box shows: one row per line in sight.
pub(crate) struct TextBoxView {
    pub(crate) rows: Vec<TextBoxRow>,
}

/// A line in the text box, as byte offsets into the title's text: where the
/// line starts, where the part in sight starts, and where it ends.
pub(crate) struct TextBoxRow {
    pub(crate) start: usize,
    pub(crate) from: usize,
    pub(crate) end: usize,
}
