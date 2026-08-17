//! Pointer gestures and playhead navigation.
//!
//! One press/move/release cycle runs through [`State::begin_drag`],
//! [`State::update_drag`] and [`State::end_drag`]. They resolve what the cursor
//! is over into a model change and hand it to `edit.rs`; the arithmetic of the
//! change itself lives there.

use crate::layout::{resolve_split, y_to_gain, y_to_opacity, PreviewHit, Splitter, TimelineHit};
use crate::state::State;
use crate::theme::*;
use crate::timeline::{Clip, FadeSide, SourceId, TrackKind, Transform};

#[derive(Copy, Clone, Debug)]
pub(crate) enum DragMode {
    None,
    Scrub,
    PoolDrag { source: SourceId },
    ClipMove { track: usize, idx: usize, grab_t_offset: f64 },
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
    /// Dragging a panel divider. Deliberately outside the undo system: where
    /// the panels sit is a view preference, and burying a real edit one step
    /// further back in the history every time you resize would be maddening.
    Splitter(Splitter),
}

impl State {
    pub(crate) fn begin_drag(&mut self) {
        // A press anywhere is the end of typing into a title. Committed rather
        // than cancelled — the click is about doing something else next, not
        // about taking the words back — and before the guard below, so the
        // typing session closes as itself rather than as a leaked drag.
        self.finish_title_edit();
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
        if self.timeline_split_btn.contains([cx, cy]) {
            self.split_at_playhead();
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
        if self.timeline_snap_btn.contains([cx, cy]) {
            self.snap_enabled = !self.snap_enabled;
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
        if self.pool_open_btn.contains([cx, cy]) {
            self.open_file_picker();
            return;
        }
        if self.pool_title_btn.contains([cx, cy]) {
            self.add_title();
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
        // Clip drags mutate continuously; snapshot once here so the whole
        // gesture collapses to one undo step (closed in `end_drag`).
        //
        // Touching a clip at all selects it — including by its trim handles,
        // since a press there is still a statement about which clip you mean.
        // Pressing bare lane or ruler clears, so there is always a way to
        // deselect without a modifier.
        match self.timeline_hit(cx, cy) {
            TimelineHit::ClipTrimLeft { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipTrimLeft { track, idx };
            }
            TimelineHit::ClipTrimRight { track, idx } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipTrimRight { track, idx };
            }
            TimelineHit::ClipBody { track, idx, grab_t_offset } => {
                self.select_clip_at(track, idx);
                self.begin_edit();
                self.drag = DragMode::ClipMove { track, idx, grab_t_offset };
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
            TimelineHit::Lane | TimelineHit::Ruler => {
                self.selected = None;
                self.drag = DragMode::Scrub;
                self.apply_scrub();
            }
            TimelineHit::None => {}
        }
    }

    pub(crate) fn update_drag(&mut self) {
        match self.drag {
            DragMode::None | DragMode::PoolDrag { .. } => {}
            DragMode::Scrub => self.apply_scrub(),
            // Store the clamped position rather than the raw cursor. Past a
            // minimum the divider stops either way, but re-clamping on write
            // means it is back under the cursor the instant you drag back,
            // instead of trailing by however far you overshot.
            // The `max` keeps a zero-sized window (minimized, on some platforms)
            // from storing a NaN fraction, which would never wash back out.
            DragMode::Splitter(Splitter::TopBottom) => {
                let h = self.logical_size()[1].max(1.0);
                let y = resolve_split(self.cursor[1] / h, h, TOP_MIN_H, TIMELINE_MIN_H);
                self.split_top_bottom = y / h;
            }
            DragMode::Splitter(Splitter::PoolPreview) => {
                let w = self.logical_size()[0].max(1.0);
                let x = resolve_split(self.cursor[0] / w, w, POOL_MIN_W, PREVIEW_MIN_W);
                self.split_pool_preview = x / w;
            }
            DragMode::ClipMove { track, idx, grab_t_offset } => {
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
                self.view_start = layout.scroll_x_to_view_start(self.cursor[0] - grab_dx);
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
        if let DragMode::PoolDrag { source } = self.drag {
            let [cx, cy] = self.cursor;
            let layout = self.timeline_layout();
            if let Some(track_idx) = self.track_at_y(cy, &layout) {
                self.begin_edit();
                let drop_t = layout.cursor_to_t(cx).max(0.0);
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

    /// One wheel notch, or one flick of a touchpad, over the timeline.
    ///
    /// Zoom is the unmodified gesture because it is the one you reach for
    /// constantly and panning has an alternative — the horizontal axis a
    /// touchpad already reports, and Shift on a wheel that has no such axis.
    /// Anywhere but the timeline it does nothing: no other panel scrolls yet,
    /// and a wheel that silently zoomed the timeline from over the media pool
    /// would read as a bug.
    pub(crate) fn wheel(&mut self, dx: f32, dy: f32) {
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
        let t = layout.cursor_to_t(self.cursor[0]);
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
