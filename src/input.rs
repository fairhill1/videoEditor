//! Pointer gestures and playhead navigation.
//!
//! One press/move/release cycle runs through [`State::begin_drag`],
//! [`State::update_drag`] and [`State::end_drag`]. They resolve what the cursor
//! is over into a model change and hand it to `edit.rs`; the arithmetic of the
//! change itself lives there.

use crate::effects::EffectKind;
use crate::fonts;
use crate::layout::{
    resolve_split, style_remove_rect, track_header_button, transform_fraction, transform_value, y_to_gain,
    y_to_opacity, EffectsHit, PickerHit, PoolTab, PoolView, TitleMenu,
    TitleControl,
    PreviewHit, Splitter, TimelineHit,
};
use crate::state::State;
use crate::theme::*;
use crate::timeline::{Clip, FadeSide, SourceId, TrackKind, Transform, TransformField};
use crate::title::{TitleNumber, SWATCHES};
use crate::ui::Rect;

#[derive(Copy, Clone, Debug)]
pub(crate) enum DragMode {
    None,
    Scrub,
    /// A press on bare lane, which a drag turns into a box that selects the
    /// clips it touches. `origin` is where the press landed; `moved` turns
    /// true once the pointer has gone far enough to mean a drag, and until
    /// then the release is a click, which puts the playhead there. `add`
    /// when Shift was down: the selection as it stood, kept in
    /// [`State::marquee_base`], is added to rather than replaced.
    Marquee { origin: [f32; 2], add: bool, moved: bool },
    PoolDrag { source: SourceId },
    /// Carrying an effect from the library to a clip on the timeline.
    EffectDrag { kind: EffectKind },
    /// Dragging one of an applied effect's sliders. `slider` is the slider's
    /// rect as the press found it, so the value follows the cursor along the
    /// same span the knob is drawn in.
    EffectParam { index: usize, param: usize, slider: Rect },
    /// Dragging one of a title's sliders on the effects tab. One undo step,
    /// opened at the press and closed in `end_drag`, as for an effect's.
    TitleNumber { field: TitleNumber, slider: Rect },
    /// Dragging one of the placement sliders, for every selected picture.
    /// One undo step, as for the others.
    Transform { field: TransformField, slider: Rect },
    /// Selecting in a title's text box by dragging across it. `field` is the
    /// box as the press found it.
    TextSelect { field: Rect },
    /// The same in the colour picker's hex box.
    HexSelect,
    /// Dragging in the colour picker's square, or along its hue strip.
    PickerSv,
    PickerHue,
    /// Moving a clip along the timeline. `group` when it was pressed as one
    /// of several selected clips: then every selected clip moves with it, and
    /// it stays on its track. `moved` turns true at the first step, so a
    /// click on a group member that never dragged can narrow the selection to
    /// that clip on release.
    ClipMove { track: usize, idx: usize, grab_t_offset: f64, group: bool, moved: bool },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
    ClipFade { track: usize, idx: usize, side: FadeSide },
    ClipLevel { track: usize, idx: usize },
    /// Placing a clip's picture on the preview canvas. Both arms carry the
    /// transform as it stood when the press landed and where the cursor was,
    /// so the drag is always measured from the grab rather than accumulated
    /// frame by frame — an accumulating drag drifts, and a clamped one drifts
    /// permanently once it has run into a limit.
    ClipMoveOnCanvas { track: usize, idx: usize, start: Transform, grab: [f32; 2] },
    /// Scaling by a corner. `anchor` is the opposite corner in canvas pixels,
    /// which stays put for the whole drag, and `corner` says which corner is
    /// being pulled. `grab` is where the press landed, also in canvas pixels,
    /// so the picture starts at exactly the size it already was however far off
    /// the corner the cursor was.
    ClipScaleOnCanvas {
        track: usize,
        idx: usize,
        start: Transform,
        grab: [f32; 2],
        anchor: [f32; 2],
        corner: [f32; 2],
    },
    /// Dragging the timeline's scrollbar. `grab_dx` is how far along the thumb
    /// the press landed, so the thumb keeps its position under the cursor
    /// instead of jumping its own left edge there.
    ///
    /// Outside the undo system, like [`DragMode::Splitter`]: where you are
    /// looking is not an edit.
    Scrollbar { grab_dx: f32 },
    /// Dragging the timeline zoom slider's knob. Outside the undo system for
    /// the same reason as the scrollbar.
    ZoomSlider,
    /// Panning the timeline with the middle button held: the time under the
    /// cursor at the press stays under it. `start` is where the view began
    /// then, so the pan is measured from the grab rather than accumulated.
    Pan { grab_x: f32, start: f64 },
    /// Dragging a panel divider. Deliberately outside the undo system: where
    /// the panels sit is saved with the project, but it is a layout rather than
    /// an edit, and burying a real edit one step further back in the history
    /// every time you resize would be maddening.
    Splitter(Splitter),
}

impl State {
    pub(crate) fn begin_drag(&mut self) {
        // A press anywhere is the end of typing into a title. Committed rather
        // than cancelled — the click is about doing something else next, not
        // about taking the words back — and before the guard below, so the
        // typing session closes as itself rather than as a leaked drag.
        self.finish_title_edit();
        // And of typing a style's name, the same way.
        self.finish_style_name();
        // A press with an edit still open means the matching release never
        // arrived (focus lost mid-drag). Close it here — otherwise the depth
        // counter leaks and every later edit nests inside it, silently
        // stalling history for the rest of the session.
        if self.edit_depth > 0 {
            self.edit_depth = 1;
            self.commit_edit();
        }
        let [cx, cy] = self.cursor;
        // The popup is drawn over everything, so it is tested before everything
        // — including the splitters, which it can overlap.
        if self.project_menu_open {
            if let Some((_, choice)) = self
                .project_menu_items
                .iter()
                .find(|(rect, _)| rect.contains([cx, cy]))
            {
                let choice = *choice;
                self.apply_project_choice(choice);
                return;
            }
            // A click anywhere else dismisses it, and is swallowed rather than
            // falling through: a press meant to close a popup should not also
            // scrub the timeline or clear the selection underneath it.
            self.project_menu_open = false;
            return;
        }
        if self.timeline_project_btn.contains([cx, cy]) {
            self.project_menu_open = true;
            return;
        }
        // The colour picker, over everything below it. An armed eyedropper
        // takes the press wherever it lands, the picker's own button aside.
        if self.color_picker.is_some() {
            let layout = self.picker_layout();
            let hit = layout.as_ref().and_then(|l| l.hit([cx, cy]));
            if self.eyedropper_armed() && hit != Some(PickerHit::Eyedropper) {
                self.eyedrop_at_cursor();
                return;
            }
            if hit != Some(PickerHit::Hex) {
                self.finish_hex_edit();
            }
            match hit {
                Some(PickerHit::Sv) => {
                    self.drag = DragMode::PickerSv;
                    self.drag_picker_sv();
                }
                Some(PickerHit::Hue) => {
                    self.drag = DragMode::PickerHue;
                    self.drag_picker_hue();
                }
                Some(PickerHit::Hex) => {
                    // The first press selects the whole code; once typing,
                    // a press puts the caret down and a drag selects.
                    if self.typing_hex() {
                        let at = self.hex_box_hit(layout.as_ref().map_or(Rect::default(), |l| l.hex), [cx, cy]);
                        self.place_hex_caret(at, self.modifiers.shift_key());
                        self.drag = DragMode::HexSelect;
                    } else {
                        self.begin_hex_edit();
                    }
                }
                Some(PickerHit::Eyedropper) => self.toggle_eyedropper(),
                Some(PickerHit::Swatch(i)) => self.pick_color(SWATCHES[i]),
                Some(PickerHit::Recent(i)) => {
                    if let Some(&c) = self.recent_colors.get(i) {
                        self.pick_color(c);
                    }
                }
                Some(PickerHit::Body) => {}
                // A press anywhere else closes it, and goes no further, as
                // for the other popups.
                None => self.close_color_picker(),
            }
            return;
        }
        // The font list, the same way: a pick, or a press anywhere else that
        // only closes it.
        if let Some((kind, menu)) = self.font_menu() {
            self.font_menu = None;
            if let Some(&(i, row)) = menu.rows.iter().find(|(_, r)| r.contains([cx, cy])) {
                match kind {
                    TitleMenu::Style if style_remove_rect(row).contains([cx, cy]) => {
                        self.delete_style(i);
                        // Left open on what remains, so clearing out a few
                        // is a few presses rather than reopening between.
                        if !self.title_styles.is_empty() {
                            self.font_menu = Some((TitleMenu::Style, menu.rows[0].0));
                        }
                    }
                    TitleMenu::Style => self.apply_style(i),
                    TitleMenu::Font => {
                        let family = fonts::choice(i).map(str::to_string);
                        self.change_shown_title(|t| t.font = family.clone());
                    }
                    TitleMenu::Weight => {
                        if let Some(&weight) = self.shown_weights().get(i) {
                            self.change_shown_title(|t| t.weight = weight);
                        }
                    }
                }
            }
            return;
        }
        self.font_menu = None;
        // Before every button and panel: a divider sits on top of the panels it
        // separates, and its grab band overlaps them by design. Nothing else
        // lives within a few points of one, so this costs the rest nothing.
        if let Some(splitter) = self.splitter_at([cx, cy]) {
            self.drag = DragMode::Splitter(splitter);
            return;
        }
        if self.transport[0].contains([cx, cy]) {
            self.goto_edit_point(false);
            return;
        }
        if self.transport[1].contains([cx, cy]) {
            self.step_frame(-1.0);
            return;
        }
        if self.transport[2].contains([cx, cy]) {
            self.toggle_playback();
            return;
        }
        if self.transport[3].contains([cx, cy]) {
            self.step_frame(1.0);
            return;
        }
        if self.transport[4].contains([cx, cy]) {
            self.goto_edit_point(true);
            return;
        }
        if self.timeline_select_all_btn.contains([cx, cy]) {
            self.select_all();
            return;
        }
        if self.timeline_deselect_btn.contains([cx, cy]) {
            self.deselect_all();
            return;
        }
        if self.timeline_split_btn.contains([cx, cy]) {
            self.split_at_playhead();
            return;
        }
        if self.timeline_freeze_btn.contains([cx, cy]) {
            self.freeze_frame();
            return;
        }
        // Consume the click even with an empty stack — undo() no-ops, and
        // falling through would start a scrub under the toolbar.
        if self.timeline_undo_btn.contains([cx, cy]) {
            self.undo();
            return;
        }
        if self.timeline_redo_btn.contains([cx, cy]) {
            self.redo();
            return;
        }
        if self.timeline_delete_btn.contains([cx, cy]) {
            self.delete_selected();
            return;
        }
        if self.timeline_copy_btn.contains([cx, cy]) {
            self.copy_selected();
            return;
        }
        if self.timeline_paste_btn.contains([cx, cy]) {
            self.paste();
            return;
        }
        if self.timeline_combine_btn.contains([cx, cy]) {
            self.combine_selected();
            return;
        }
        if self.timeline_open_compound_btn.contains([cx, cy]) {
            self.open_selected_compound();
            return;
        }
        if self.timeline_back_btn.contains([cx, cy]) {
            self.close_compound();
            return;
        }
        if self.timeline_snap_btn.contains([cx, cy]) {
            self.snap_enabled = !self.snap_enabled;
            return;
        }
        if self.timeline_ripple_btn.contains([cx, cy]) {
            self.ripple_enabled = !self.ripple_enabled;
            return;
        }
        if self.timeline_zoom_out_btn.contains([cx, cy]) {
            self.zoom_at_playhead(-1.0);
            return;
        }
        if self.timeline_zoom_in_btn.contains([cx, cy]) {
            self.zoom_at_playhead(1.0);
            return;
        }
        if self.timeline_zoom_fit_btn.contains([cx, cy]) {
            self.zoom_timeline_to_fit();
            return;
        }
        if self.timeline_zoom_slider.contains([cx, cy]) {
            // The knob jumps to the press, then follows the drag.
            self.drag = DragMode::ZoomSlider;
            self.update_drag();
            return;
        }
        if self.timeline_export_btn.contains([cx, cy]) {
            self.start_export();
            return;
        }
        if self.timeline_open_btn.contains([cx, cy]) {
            self.open_project();
            return;
        }
        // Consumed even when the project is clean and the button is greyed, for
        // the same reason as undo: falling through would scrub the timeline
        // underneath the toolbar.
        if self.timeline_reload_btn.contains([cx, cy]) {
            self.reload_project();
            return;
        }
        if self.timeline_save_btn.contains([cx, cy]) {
            if self.dirty {
                self.save_project(false);
            }
            return;
        }
        // Before the panels below, and after the toolbar above: the canvas is
        // an island inside the preview well, and nothing else is drawn over it.
        if let Some((track, idx, hit)) = self.preview_hit([cx, cy]) {
            let start = self.timeline.tracks[track].clips[idx].transform;
            self.begin_edit();
            self.drag = match hit {
                PreviewHit::Move => DragMode::ClipMoveOnCanvas {
                    track,
                    idx,
                    start,
                    grab: [cx, cy],
                },
                PreviewHit::Scale { corner } => {
                    let source = self.timeline.tracks[track].clips[idx].source;
                    let canvas = self.canvas();
                    let (Some((sw, sh)), Some(grab)) =
                        (self.source_size(source, canvas), self.cursor_on_canvas())
                    else {
                        return;
                    };
                    let [x, y, w, h] = canvas.place(sw as f32, sh as f32, start);
                    DragMode::ClipScaleOnCanvas {
                        track,
                        idx,
                        start,
                        grab,
                        // The corner across the picture from the one grabbed.
                        anchor: [
                            x + (1.0 - corner[0]) * w,
                            y + (1.0 - corner[1]) * h,
                        ],
                        corner,
                    }
                }
            };
            return;
        }
        for (rect, tab) in self.pool_tab_btns.into_iter().zip([PoolTab::Media, PoolTab::Effects]) {
            if rect.contains([cx, cy]) {
                if self.pool_tab != tab {
                    self.pool_tab = tab;
                    // Saved with the project's layout.
                    self.dirty = true;
                }
                return;
            }
        }
        match self.pool_tab {
            PoolTab::Media => {
                if self.pool_open_btn.contains([cx, cy]) {
                    self.open_file_picker();
                    return;
                }
                if self.pool_title_btn.contains([cx, cy]) {
                    self.add_title();
                    return;
                }
                if self.pool_view_btn.contains([cx, cy]) {
                    self.pool_view = match self.pool_view {
                        PoolView::List => PoolView::Grid,
                        PoolView::Grid => PoolView::List,
                    };
                    self.dirty = true;
                    return;
                }
                if let Some(id) = self.pool_close_hit(cx, cy) {
                    self.remove_source(id);
                    return;
                }
                if let Some(source) = self.pool_hit(cx, cy) {
                    self.drag = DragMode::PoolDrag { source };
                    return;
                }
            }
            PoolTab::Effects if cx < self.media_pool_w() && cy < self.timeline_top() => {
                match self.effects_panel().hit([cx, cy]) {
                    Some(EffectsHit::Library(kind)) => self.drag = DragMode::EffectDrag { kind },
                    Some(EffectsHit::Apply(kind)) => self.apply_effect_to_selection(kind),
                    Some(EffectsHit::Toggle { index }) => self.toggle_effect(index),
                    Some(EffectsHit::Remove { index }) => self.remove_effect(index),
                    Some(EffectsHit::Param { index, param, slider }) => {
                        // One undo step for the whole drag, closed in
                        // `end_drag`; the value jumps to the press first.
                        self.begin_edit();
                        self.drag = DragMode::EffectParam { index, param, slider };
                        self.update_drag();
                    }
                    Some(EffectsHit::TitleText) => {
                        // The caret goes where the click is, worked out on the
                        // box as it was drawn — scrolled to the old caret if
                        // typing was already under way.
                        let panel = self.effects_panel();
                        let field = panel.title.iter().find(|r| r.control == TitleControl::Text).map(|r| r.field);
                        if let (Some((source, title)), Some(field)) = (self.effects_title(), field) {
                            let typing = self.editing_title.as_ref().is_some_and(|(s, _)| *s == source);
                            let caret = typing.then_some(self.title_cursor.caret);
                            let at = self.text_box_hit(field, &title.text, caret, [cx, cy]);
                            if !typing {
                                self.begin_title_edit(source);
                            }
                            // Shift extends what is selected; a plain press
                            // puts the caret down, and dragging from it
                            // selects.
                            let extend = typing && self.modifiers.shift_key();
                            self.title_cursor.move_to(at, extend);
                            self.drag = DragMode::TextSelect { field };
                        }
                    }
                    Some(EffectsHit::TitleStyle) => self.open_style_menu(),
                    Some(EffectsHit::TitleSaveStyle) => self.save_shown_style(),
                    Some(EffectsHit::TitleFont) => self.open_font_menu(),
                    Some(EffectsHit::TitleWeight) => self.open_weight_menu(),
                    Some(EffectsHit::TitleNumber { field, slider }) => {
                        self.begin_edit();
                        self.drag = DragMode::TitleNumber { field, slider };
                        self.update_drag();
                    }
                    Some(EffectsHit::Transform { field, slider }) => {
                        self.begin_edit();
                        self.drag = DragMode::Transform { field, slider };
                        self.update_drag();
                    }
                    Some(EffectsHit::TransformReset) => self.reset_selected_transform(),
                    Some(EffectsHit::TitleColorChip(field)) => self.open_color_picker(field),
                    Some(EffectsHit::TitleSwatch { field, swatch }) => {
                        self.change_shown_title(|t| field.set(t, SWATCHES[swatch]));
                    }
                    Some(EffectsHit::TitleToggle(part)) => {
                        // Every styled title to the opposite of the shown
                        // one, rather than each flipped from its own.
                        if let Some(on) = self.effects_title().map(|(_, t)| !part.enabled(t)) {
                            self.change_shown_title(|t| part.set(t, on));
                        }
                    }
                    None => {}
                }
                return;
            }
            PoolTab::Effects => {}
        }
        // Track headers: the + in each add row, and the remove on an empty
        // lane. They sit left of the clip band, so nothing below competes.
        let layout = self.timeline_layout();
        for (kind, row) in self.track_add_rows(&layout) {
            if track_header_button(row.y, row.h).contains([cx, cy]) {
                self.add_track(kind);
                return;
            }
        }
        if let Some((idx, _)) = self
            .track_remove_buttons(&layout)
            .into_iter()
            .find(|(_, r)| r.contains([cx, cy]))
        {
            self.remove_track(idx);
            return;
        }
        for (idx, [up, down]) in self.track_move_buttons(&layout) {
            if up.contains([cx, cy]) || down.contains([cx, cy]) {
                self.move_track(idx, up.contains([cx, cy]));
                return;
            }
        }
        // The strip along the bottom belongs to the scrollbar whether or not
        // there is a thumb in it: a press there must not fall through to the
        // lane above, which `track_at_y` would otherwise hand it to.
        let layout = self.timeline_layout();
        if layout.scrollbar_rect().contains([cx, cy]) {
            if let Some(grab_dx) = layout.scroll_grab_offset(cx) {
                self.drag = DragMode::Scrollbar { grab_dx };
                // Applied straight away so a press beside the thumb jumps the
                // view without waiting for the cursor to move.
                self.update_drag();
            }
            return;
        }
        // What is left of a header once its buttons have had their say: a
        // press there pins the track as where pastes go, or lets it go.
        if let Some(idx) = self.track_header_at([cx, cy], &layout) {
            self.toggle_paste_pin(idx);
            return;
        }
        // Clip drags mutate continuously; snapshot once here so the whole
        // gesture collapses to one undo step (closed in `end_drag`).
        //
        // Touching a clip at all selects it — including by its trim handles,
        // since a press there is still a statement about which clip you mean.
        // Pressing bare lane or ruler clears, so there is always a way to
        // deselect without a modifier.
        //
        // Shift+press on a clip adds it to the selection or takes it out, and
        // that is all it does: a press that is busy building a selection is not
        // also grabbing a clip to move, and a drag that set off from it would
        // move one clip of several, which is not what a group looks like it
        // is about to do.
        let hit = self.timeline_hit(cx, cy);
        let shift = self.modifiers.shift_key();
        if shift {
            if let TimelineHit::ClipTrimLeft { track, idx }
            | TimelineHit::ClipTrimRight { track, idx }
            | TimelineHit::ClipBody { track, idx, .. }
            | TimelineHit::ClipFade { track, idx, .. }
            | TimelineHit::ClipLevel { track, idx }
            | TimelineHit::ClipEffects { track, idx } = hit
            {
                self.toggle_clip_at(track, idx);
                return;
            }
        }
        match hit {
            TimelineHit::ClipTrimLeft { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.begin_ripple_trim(track, idx);
                self.drag = DragMode::ClipTrimLeft { track, idx };
            }
            TimelineHit::ClipTrimRight { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.begin_ripple_trim(track, idx);
                self.drag = DragMode::ClipTrimRight { track, idx };
            }
            TimelineHit::ClipBody { track, idx, grab_t_offset } => {
                let id = self.timeline.tracks[track].clips[idx].id;
                let now = std::time::Instant::now();
                let double = self.last_clip_click.is_some_and(|(last, at)| {
                    last == id && now.duration_since(at).as_millis() <= DOUBLE_CLICK_MS
                });
                self.last_clip_click = (!double).then_some((id, now));
                if double {
                    self.open_clip(track, idx);
                    return;
                }
                // A press on one of several selected clips keeps them all
                // selected, so the drag that follows takes the group.
                let group = self.is_selected(id) && self.selection_with_links().len()
                    > self.linked_siblings(track, idx).len();
                if !group {
                    self.select_clip_at(track, idx);
                }
                self.begin_edit();
                self.drag = DragMode::ClipMove { track, idx, grab_t_offset, group, moved: false };
            }
            TimelineHit::ClipFade { track, idx, side } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipFade { track, idx, side };
            }
            // Only reachable on a clip that is already selected — see the note
            // in `timeline_hit` — so there is no selection to make here.
            TimelineHit::ClipLevel { track, idx } => {
                self.begin_edit();
                self.drag = DragMode::ClipLevel { track, idx };
            }
            // The chain is a switch. It picks the clip as well, so what comes
            // next — a delete, most likely — is done to this half.
            TimelineHit::ClipLink { track, idx } => {
                self.select_clip_at(track, idx);
                self.toggle_link(track, idx);
            }
            // The badge is the way from a clip to its effects: pick the clip,
            // and turn the panel to the page that edits them.
            TimelineHit::ClipEffects { track, idx } => {
                self.select_clip_at(track, idx);
                if self.pool_tab != PoolTab::Effects {
                    self.pool_tab = PoolTab::Effects;
                    self.dirty = true;
                }
            }
            // Bare lane is where a box selection starts. Nothing happens
            // until the pointer has moved or let go: a release that never
            // dragged is the click that puts the playhead here, in
            // `end_drag`.
            TimelineHit::Lane => {
                self.marquee_base = if shift { self.selected.clone() } else { Vec::new() };
                self.drag = DragMode::Marquee { origin: [cx, cy], add: shift, moved: false };
            }
            TimelineHit::Ruler => {
                // Shift keeps the selection, so the playhead can be moved to
                // where a group of clips is about to be cut.
                if !shift {
                    self.selected.clear();
                }
                self.drag = DragMode::Scrub;
                self.apply_scrub();
            }
            TimelineHit::None => {}
        }
    }

    pub(crate) fn update_drag(&mut self) {
        match self.drag {
            DragMode::None | DragMode::PoolDrag { .. } | DragMode::EffectDrag { .. } => {}
            DragMode::EffectParam { index, param, slider } => {
                let Some((track, idx)) = self.effects_target() else {
                    return;
                };
                let clip = &self.timeline.tracks[track].clips[idx];
                let Some(spec) = clip.effects.get(index).and_then(|e| e.kind().params().get(param))
                else {
                    return;
                };
                let value = spec.at(slider_fraction(slider, self.cursor[0]) as f32);
                self.set_effect_param(index, param, value);
            }
            DragMode::TextSelect { field } => {
                let caret = Some(self.title_cursor.caret);
                if let Some((_, title)) = self.effects_title() {
                    let at = self.text_box_hit(field, &title.text.clone(), caret, self.cursor);
                    self.title_cursor.move_to(at, true);
                }
            }
            DragMode::HexSelect => {
                if let Some(layout) = self.picker_layout() {
                    let at = self.hex_box_hit(layout.hex, self.cursor);
                    self.place_hex_caret(at, true);
                }
            }
            DragMode::PickerSv => self.drag_picker_sv(),
            DragMode::PickerHue => self.drag_picker_hue(),
            DragMode::TitleNumber { field, slider } => {
                let value = field.spec().at(slider_fraction(slider, self.cursor[0]) as f32);
                self.edit_shown_title(|t| field.set(t, value));
            }
            DragMode::Transform { field, slider } => {
                let Some((track, idx)) = self.effects_target() else {
                    return;
                };
                let clip = self.timeline.tracks[track].clips[idx];
                let reach = self.transform_reach(&clip);
                let fraction = slider_fraction(slider, self.cursor[0]) as f32;
                // Centred, and 100%, are where a placement most often wants
                // to be put back to, and a slider is hard to stop on exactly.
                let rest = if field == TransformField::Zoom { 1.0 } else { 0.0 };
                let value = if self.snap_enabled
                    && (fraction - transform_fraction(field, rest, reach)).abs() < TRANSFORM_SLIDER_SNAP
                {
                    rest
                } else {
                    transform_value(field, fraction, reach)
                };
                self.set_selected_transform(field, value);
            }
            DragMode::Scrub => self.apply_scrub(),
            DragMode::Marquee { origin, add, moved } => {
                let far = (self.cursor[0] - origin[0]).abs().max((self.cursor[1] - origin[1]).abs());
                if !moved && far < MARQUEE_START_PX {
                    return;
                }
                self.drag = DragMode::Marquee { origin, add, moved: true };
                // Worked out afresh from the box each time, so a clip the
                // box has backed off is let go again.
                let mut picked = self.marquee_base.clone();
                for id in self.clips_in_box(origin, self.cursor) {
                    if !picked.contains(&id) {
                        picked.push(id);
                    }
                }
                self.selected = picked;
            }
            DragMode::Pan { grab_x, start } => {
                let px_per_sec = self.timeline_layout().px_per_sec();
                if px_per_sec > 0.0 {
                    self.scroll_view_to(start - (self.cursor[0] - grab_x) as f64 / px_per_sec);
                }
            }
            DragMode::ZoomSlider => {
                let fraction = slider_fraction(self.timeline_zoom_slider, self.cursor[0]);
                self.zoom_timeline_to_fraction(fraction);
            }
            // Store the clamped position rather than the raw cursor. Past a
            // minimum the divider stops either way, but re-clamping on write
            // means it is back under the cursor the instant you drag back,
            // instead of trailing by however far you overshot.
            // The `max` keeps a zero-sized window (minimized, on some platforms)
            // from storing a NaN fraction, which would never wash back out.
            DragMode::Splitter(Splitter::TopBottom) => {
                let h = self.logical_size()[1].max(1.0);
                let y = resolve_split(self.cursor[1] / h, h, TOP_MIN_H, TIMELINE_MIN_H);
                // Saved with the project, so moving it is a change worth saving
                // — though not an edit worth undoing.
                if self.split_top_bottom != y / h {
                    self.split_top_bottom = y / h;
                    self.dirty = true;
                }
            }
            DragMode::Splitter(Splitter::PoolPreview) => {
                let w = self.logical_size()[0].max(1.0);
                let x = resolve_split(self.cursor[0] / w, w, POOL_MIN_W, PREVIEW_MIN_W);
                if self.split_pool_preview != x / w {
                    self.split_pool_preview = x / w;
                    self.dirty = true;
                }
            }
            DragMode::ClipMove { track, idx, grab_t_offset, group, .. } if group => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start = self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = (cursor_t - grab_t_offset) - current_start;
                if desired_delta != 0.0 {
                    self.drag = DragMode::ClipMove { track, idx, grab_t_offset, group, moved: true };
                }
                let ids = self.selection_with_links();
                let set: Vec<(usize, usize)> =
                    ids.iter().filter_map(|id| self.timeline.find(*id)).collect();
                self.apply_group_move_delta(&set, desired_delta);
            }
            DragMode::ClipMove { track, idx, grab_t_offset, .. } => {
                let layout = self.timeline_layout();
                // Allow vertical drag: if the cursor is over a different
                // same-kind track, relocate the clip there before nudging x.
                // Cross-kind moves (V↔A) are blocked — a video clip on an
                // audio lane has no meaningful playback behavior yet. Linked
                // siblings stay on their own tracks; only the dragged clip
                // changes track membership.
                let (track, idx) = if let Some(hover) = self.track_at_y(self.cursor[1], &layout) {
                    let src_kind = self.timeline.tracks[track].kind;
                    let dst_kind = self.timeline.tracks[hover].kind;
                    if hover != track && src_kind == dst_kind {
                        let clip = self.timeline.tracks[track].clips.remove(idx);
                        let new_idx = self.timeline.tracks[hover].clips.len();
                        self.timeline.tracks[hover].clips.push(clip);
                        self.drag = DragMode::ClipMove {
                            track: hover,
                            idx: new_idx,
                            grab_t_offset,
                            group: false,
                            moved: true,
                        };
                        (hover, new_idx)
                    } else {
                        (track, idx)
                    }
                } else {
                    (track, idx)
                };
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start = self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = (cursor_t - grab_t_offset) - current_start;
                self.apply_move_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimLeft { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                if self.ripple_trim.is_some() {
                    self.apply_ripple_trim_left(track, idx, cursor_t);
                    return;
                }
                let current_start = self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = cursor_t - current_start;
                self.apply_trim_left_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimRight { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_end = self.timeline.tracks[track].clips[idx].timeline_end();
                let desired_delta = cursor_t - current_end;
                self.apply_trim_right_delta(track, idx, desired_delta);
            }
            // A fade is set by where its far end lands, not by how far the
            // cursor has moved: grabbing the handle anywhere within its box and
            // dragging puts the end of the ramp under the cursor.
            DragMode::ClipFade { track, idx, side } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let clip = self.timeline.tracks[track].clips[idx];
                let len = match side {
                    FadeSide::In => cursor_t - clip.timeline_start,
                    FadeSide::Out => clip.timeline_end() - cursor_t,
                };
                self.set_clip_fade(track, idx, side, len);
            }
            // A drag across the canvas moves the picture by the same distance
            // the cursor travelled — converted through the preview's own scale,
            // so the picture stays under the finger at any panel size, and
            // stored as a fraction of the canvas so it stays put if the project
            // is later re-mastered larger.
            DragMode::ClipMoveOnCanvas { track, idx, start, grab } => {
                let canvas = self.canvas();
                let s = self.preview_canvas_scale;
                if s <= 0.0 {
                    return;
                }
                let wanted = Transform {
                    x: start.x + (self.cursor[0] - grab[0]) / s / canvas.width as f32,
                    y: start.y + (self.cursor[1] - grab[1]) / s / canvas.height as f32,
                    ..start
                };
                let source = self.timeline.tracks[track].clips[idx].source;
                let placed = match self.source_size(source, canvas) {
                    Some(size) => self.snap_transform_offset(size, wanted),
                    None => wanted,
                };
                self.set_clip_transform(track, idx, placed);
            }
            // Scale follows how much further from the pinned corner the cursor
            // has travelled, projected onto the direction it was grabbed in. A
            // bare distance ratio would have the picture swelling when the
            // cursor moved sideways past the corner; the projection only counts
            // movement along the line the grab established, which is also what
            // keeps a uniform scale honest on a diagonal drag.
            DragMode::ClipScaleOnCanvas { track, idx, start, grab, anchor, corner } => {
                let Some(cursor) = self.cursor_on_canvas() else {
                    return;
                };
                let v0 = [grab[0] - anchor[0], grab[1] - anchor[1]];
                let v = [cursor[0] - anchor[0], cursor[1] - anchor[1]];
                let den = v0[0] * v0[0] + v0[1] * v0[1];
                // A press within a pixel of the pinned corner says nothing
                // about a direction, and dividing by it would send the scale to
                // whichever limit the noise pointed at.
                if den < 1.0 {
                    return;
                }
                let factor = (v[0] * v0[0] + v[1] * v0[1]) / den;
                let source = self.timeline.tracks[track].clips[idx].source;
                let Some(size) = self.source_size(source, self.canvas()) else {
                    return;
                };
                let scale = self.snap_scale_about(size, anchor, corner, start.scale * factor);
                let placed = self.transform_scaled_about(size, anchor, corner, scale);
                self.set_clip_transform(track, idx, placed);
            }
            DragMode::Scrollbar { grab_dx } => {
                let layout = self.timeline_layout();
                self.scroll_view_to(layout.scroll_x_to_view_start(self.cursor[0] - grab_dx));
            }
            // One gesture, two quantities: the line under the cursor carries
            // the sound's level on an audio lane and the picture's opacity on a
            // video one, and which of the two it is follows the lane rather
            // than anything the drag has to be told.
            DragMode::ClipLevel { track, idx } => {
                let layout = self.timeline_layout();
                let lane_y = self.lane_top(track, &layout);
                match self.timeline.tracks[track].kind {
                    TrackKind::Audio => {
                        let gain = y_to_gain(lane_y, layout.lane_h, self.cursor[1]);
                        self.set_clip_gain(track, idx, gain);
                    }
                    TrackKind::Video => {
                        let opacity = y_to_opacity(lane_y, layout.lane_h, self.cursor[1]);
                        self.set_clip_opacity(track, idx, opacity);
                    }
                }
            }
        }
    }

    pub(crate) fn end_drag(&mut self) {
        self.ripple_trim = None;
        // A press on bare lane that never became a box is a click: it lets
        // the selection go, unless Shift is holding it, and puts the
        // playhead where it landed.
        if let DragMode::Marquee { add, moved: false, .. } = self.drag {
            if !add {
                self.selected.clear();
            }
            self.apply_scrub();
        }
        // A click on one of several selected clips that never became a drag
        // picks that clip alone, as a click on any other clip does.
        if let DragMode::ClipMove { track, idx, group: true, moved: false, .. } = self.drag {
            if track < self.timeline.tracks.len() && idx < self.timeline.tracks[track].clips.len() {
                self.select_clip_at(track, idx);
            }
        }
        // Dropped on a clip: the effect goes on that clip's picture, and the
        // clip is selected so the panel the effect came from shows it there.
        if let DragMode::EffectDrag { kind } = self.drag {
            if let Some(target) = self.effect_drop_target() {
                self.apply_effect(kind, &[target]);
                self.select_clip_at(target.0, target.1);
            }
        }
        if let DragMode::PoolDrag { source } = self.drag {
            if !self.may_place(source) && self.cursor[1] >= self.timeline_top() {
                self.refuse_nesting();
                self.drag = DragMode::None;
                return;
            }
            let cy = self.cursor[1];
            let layout = self.timeline_layout();
            // Opened before a track can be made, so a new track and the clip
            // that asked for it are one step to undo.
            self.begin_edit();
            let target = self.track_at_y(cy, &layout).or_else(|| {
                self.new_track_drop(source, self.cursor)
                    .map(|kind| self.timeline.push_track(kind))
            });
            if let Some(track_idx) = target {
                let drop_t = self.pool_drop_start(source);
                let kind = self.timeline.tracks[track_idx].kind;
                match kind {
                    // Mirrors the audio lane's rule below: a source with no
                    // picture has nothing to play on a video track, so the drop
                    // is a no-op rather than a clip that renders as nothing.
                    TrackKind::Video if !self.media.has_video(source) => {}
                    TrackKind::Video => {
                        let dur = self.media.drop_duration(source);
                        // Decide up front whether we're auto-pairing audio —
                        // only then do we need a link id, and both sides must
                        // use the same one.
                        //
                        // The lane is chosen for the user rather than pointed
                        // at, so it has to be one with room: dropping onto A1
                        // whatever is already there would stack two clips over
                        // the same instant, and the mixer plays whichever of
                        // them it reaches first. A fresh track when every
                        // existing one is busy — the alternative is silently
                        // burying the audio the drop just asked for.
                        let audio_target = self.media.audio_duration(source).map(|adur| {
                            self.timeline
                                .free_audio_track(drop_t, drop_t + adur)
                                .unwrap_or_else(|| self.timeline.push_track(TrackKind::Audio))
                        });
                        let link = audio_target.map(|_| self.timeline.new_link_id());
                        let id = self.timeline.new_clip_id();
                        self.timeline.tracks[track_idx].clips.push(Clip {
                            id,
                            source,
                            source_out: dur,
                            timeline_start: drop_t,
                            link,
                            ..Clip::default()
                        });
                        if let Some(audio_idx) = audio_target {
                            let adur = self.media.audio_duration(source).unwrap_or(dur);
                            let id = self.timeline.new_clip_id();
                            self.timeline.tracks[audio_idx].clips.push(Clip {
                                id,
                                source,
                                source_out: adur,
                                timeline_start: drop_t,
                                link,
                                ..Clip::default()
                            });
                        }
                    }
                    TrackKind::Audio => {
                        if let Some(adur) = self.media.audio_duration(source) {
                            let id = self.timeline.new_clip_id();
                            self.timeline.tracks[track_idx].clips.push(Clip {
                                id,
                                source,
                                source_out: adur,
                                timeline_start: drop_t,
                                ..Clip::default()
                            });
                        }
                        // Dropping a video-only source on an audio lane is a
                        // no-op — there's nothing to play back there.
                    }
                }
            }
        }
        // Closes whichever edit `begin_drag` or the pool drop opened; a no-op
        // gesture (click without move, drop that landed nowhere) discards it.
        self.commit_edit();
        self.drag = DragMode::None;
    }

    /// The middle button going down: over the timeline, a pan. Anywhere else,
    /// or with another drag already running, nothing.
    pub(crate) fn begin_pan(&mut self) {
        if !matches!(self.drag, DragMode::None) || self.cursor[1] < self.timeline_top() {
            return;
        }
        let (start, _) = self.view_window();
        self.drag = DragMode::Pan { grab_x: self.cursor[0], start };
    }

    pub(crate) fn end_pan(&mut self) {
        if matches!(self.drag, DragMode::Pan { .. }) {
            self.drag = DragMode::None;
        }
    }

    /// The video clip a dragged effect would land on if let go here: the one
    /// under the cursor, or the picture of the audio clip under it.
    pub(crate) fn effect_drop_target(&self) -> Option<(usize, usize)> {
        let (track, idx) = match self.timeline_hit(self.cursor[0], self.cursor[1]) {
            TimelineHit::ClipBody { track, idx, .. }
            | TimelineHit::ClipTrimLeft { track, idx }
            | TimelineHit::ClipTrimRight { track, idx }
            | TimelineHit::ClipFade { track, idx, .. }
            | TimelineHit::ClipLevel { track, idx }
            | TimelineHit::ClipEffects { track, idx }
            | TimelineHit::ClipLink { track, idx } => (track, idx),
            _ => return None,
        };
        self.effects_clip_for(track, idx)
    }

    /// One wheel notch, or one flick of a touchpad, over the timeline.
    ///
    /// Zoom is the unmodified gesture because it is the one you reach for
    /// constantly and panning has an alternative — the horizontal axis a
    /// touchpad already reports, and Shift on a wheel that has no such axis.
    /// Anywhere but the timeline it does nothing: no other panel scrolls yet,
    /// and a wheel that silently zoomed the timeline from over the media pool
    /// would read as a bug.
    pub(crate) fn wheel(&mut self, dx: f32, dy: f32) {
        // The font list first: it is drawn over the panels, so the wheel over
        // it means it.
        if let Some((kind, menu)) = self.font_menu() {
            if menu.rect.contains(self.cursor) {
                let scroll = self.font_menu.map_or(0, |(_, s)| s);
                let rows = scroll.min(menu.max_scroll) as f32 - dy.round();
                self.font_menu = Some((kind, (rows.max(0.0) as usize).min(menu.max_scroll)));
                return;
            }
        }
        let [cx, cy] = self.cursor;
        if self.pool_tab == PoolTab::Effects && cx < self.media_pool_w() && cy < self.timeline_top() {
            // Down the page for a wheel turned towards you, as everywhere.
            let max = self.effects_panel().max_scroll;
            self.effects_scroll = (self.effects_scroll - dy * EFFECTS_SCROLL_STEP).clamp(0.0, max);
            return;
        }
        if self.cursor[1] < self.timeline_top() {
            return;
        }
        if dx != 0.0 {
            self.pan_timeline(dx as f64);
        }
        if dy != 0.0 {
            if self.modifiers.shift_key() {
                // Down pans right, matching the direction the same gesture
                // scrolls a page.
                self.pan_timeline(-dy as f64);
            } else {
                self.zoom_timeline(dy as f64, self.cursor[0]);
            }
        }
    }

    /// Zoom about the playhead rather than the cursor, for the keyboard: the
    /// hand on the keys has said nothing about where the pointer is, and the
    /// playhead is the position the rest of the keymap works around.
    pub(crate) fn zoom_at_playhead(&mut self, steps: f64) {
        let layout = self.timeline_layout();
        let x = layout
            .t_to_x(self.audio.position())
            .clamp(layout.clips_x, layout.clips_right());
        self.zoom_timeline(steps, x);
    }

    fn apply_scrub(&mut self) {
        let duration = self.timeline.duration();
        if duration <= 0.0 {
            return;
        }
        let layout = self.timeline_layout();
        // The view runs on past the last clip; the playhead does not.
        let t = layout.cursor_to_t(self.cursor[0]).min(duration);
        // Audio engine owns the playhead — setting position also flushes any
        // pre-mixed samples so the next tick refills from the new time,
        // keeping scrub snappy instead of dragging 150ms of stale audio.
        self.audio.set_position(t);
    }

    /// Frame stepping follows the canvas, not the clip under the playhead: a
    /// frame is a unit of the project, and on a mixed-rate timeline stepping by
    /// whatever happens to be underneath made a "frame" change length partway
    /// along the sequence.
    fn current_fps(&self) -> f64 {
        self.canvas().fps
    }

    pub(crate) fn step_frame(&mut self, dir: f64) {
        if self.audio.playing() {
            self.audio.set_playing(false);
        }
        let fps = self.current_fps().max(1.0);
        let dt = dir / fps;
        let mut new_t = (self.audio.position() + dt).max(0.0);
        let duration = self.timeline.duration();
        if duration > 0.0 {
            new_t = new_t.min(duration);
        }
        self.audio.set_position(new_t);
    }

    /// Jump to the clip boundary before/after the playhead. Pauses like
    /// `step_frame` does — these sit in the same row of navigation buttons and
    /// splitting the pause behavior between them would be arbitrary.
    pub(crate) fn goto_edit_point(&mut self, forward: bool) {
        let t = self.audio.position();
        let target = if forward {
            self.timeline.next_edit_point(t)
        } else {
            self.timeline.prev_edit_point(t)
        };
        let Some(target) = target else {
            return;
        };
        if self.audio.playing() {
            self.audio.set_playing(false);
        }
        self.audio.set_position(target);
    }

    pub(crate) fn toggle_playback(&mut self) {
        // Starting from the end replays from the top rather than doing nothing.
        // The render loop parks the playhead exactly on the duration when
        // playback reaches it, so a bare toggle would set `playing` and be
        // stopped again on the very next frame — a keypress that looks broken.
        //
        // Exact compare rather than a tolerance: that parking is the only thing
        // that puts the playhead at the end, and it assigns the duration
        // itself. A pause a hair short of the end is a real position, and
        // resuming from it is what the user asked for.
        if !self.audio.playing() {
            let duration = self.timeline.duration();
            if duration > 0.0 && self.audio.position() >= duration {
                self.audio.set_position(0.0);
            }
        }
        self.audio.toggle();
    }
}

/// How far along `slider` a cursor at `x` puts the knob's centre, as 0 to 1.
/// The knob's own half-width is taken off each end, so the extremes are
/// reached with the knob still wholly inside the slider.
pub(crate) fn slider_fraction(slider: Rect, x: f32) -> f64 {
    let travel = (slider.w - ZOOM_SLIDER_KNOB_W).max(1.0);
    (((x - slider.x - ZOOM_SLIDER_KNOB_W * 0.5) / travel) as f64).clamp(0.0, 1.0)
}
