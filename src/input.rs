//! Pointer gestures and playhead navigation.
//!
//! One press/move/release cycle runs through [`State::begin_drag`],
//! [`State::update_drag`] and [`State::end_drag`]. They resolve what the cursor
//! is over into a model change and hand it to `edit.rs`; the arithmetic of the
//! change itself lives there.

use crate::layout::{resolve_split, y_to_gain, Splitter, TimelineHit};
use crate::state::State;
use crate::theme::*;
use crate::timeline::{Clip, FadeSide, SourceId, TrackKind};

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
    /// Dragging a panel divider. Deliberately outside the undo system: where
    /// the panels sit is a view preference, and burying a real edit one step
    /// further back in the history every time you resize would be maddening.
    Splitter(Splitter),
}

impl State {
    pub(crate) fn begin_drag(&mut self) {
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
        if self.pool_open_btn.contains([cx, cy]) {
            self.open_file_picker();
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
            DragMode::ClipLevel { track, idx } => {
                let layout = self.timeline_layout();
                let lane_y = self.lane_top(track, &layout);
                let gain = y_to_gain(lane_y, layout.lane_h, self.cursor[1]);
                self.set_clip_gain(track, idx, gain);
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
                        let dur = self.media.duration(source);
                        // Decide up front whether we're auto-pairing audio —
                        // only then do we need a link id, and both sides must
                        // use the same one.
                        let audio_target = self
                            .media
                            .has_audio(source)
                            .then(|| {
                                self.timeline
                                    .tracks
                                    .iter()
                                    .position(|t| t.kind == TrackKind::Audio)
                            })
                            .flatten();
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
