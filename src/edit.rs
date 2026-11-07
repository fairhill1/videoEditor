//! Edits to the timeline, and the history that can take them back.
//!
//! Everything here mutates the model and nothing here reads the cursor: the
//! gestures that drive these live in `input.rs` and arrive as already-resolved
//! deltas. That split is what lets a keyboard shortcut and a drag share one
//! implementation of "move this clip".

use crate::effects::{Effect, EffectKind, MAX_EFFECTS};
use crate::input::DragMode;
use crate::state::State;
use crate::fonts;
use crate::layout::{PoolTab, TitleMenu};
use crate::theme::{FONT_MENU_ROWS, SNAP_PX, STATUS_INFO};
use crate::title::{self, Title};
use crate::timeline::{
    db_to_gain, gain_to_db, FadeSide, SourceId, TimelineSnapshot, TrackKind, Tracks, Transform,
    MAX_GAIN_DB, MAX_OPACITY, MAX_SCALE, MIN_GAIN_DB, MIN_OPACITY, MIN_SCALE,
};

/// Keyboard steps for the level line, coarse and fine. Audio moves in decibels
/// and the picture in opacity, so the two have their own pairs rather than one
/// number that would mean something different on each kind of lane.
const GAIN_STEP_DB: f32 = 1.0;
const GAIN_STEP_FINE_DB: f32 = 0.1;
const OPACITY_STEP: f32 = 0.05;
const OPACITY_STEP_FINE: f32 = 0.01;

/// Keeps trim from zeroing a clip, in seconds.
pub(crate) const MIN_CLIP_DURATION: f64 = 0.05;

/// Retained undo steps. Snapshots are small, but a long session shouldn't grow
/// without bound; the oldest step is dropped past this.
const UNDO_LIMIT: usize = 200;

/// One undo step: everything a user edit can change. Pool membership rides
/// along with the timeline because deleting a pool item also deletes its
/// clips — undoing that has to put both back in one move. Titles ride along for
/// the same reason: what a title says is part of the edit, not a property of
/// some file on disk that undo has no business in.
#[derive(Clone, PartialEq)]
pub(crate) struct EditSnapshot {
    timeline: TimelineSnapshot,
    pool_order: Vec<SourceId>,
    titles: Vec<(SourceId, Title)>,
    compounds: Vec<(SourceId, Tracks)>,
}

/// Closest latch of any `edge` onto any `target` within `threshold`, expressed
/// as the offset to add to the drag delta. `None` when nothing is in range.
///
/// Every edge competes against every target and the smallest gap wins, so a
/// clip latches by whichever end you brought near a neighbour rather than by a
/// fixed leading edge.
fn nearest_snap(edges: &[f64], targets: &[f64], threshold: f64) -> Option<f64> {
    let mut best: Option<(f64, f64)> = None;
    for &edge in edges {
        for &target in targets {
            let dist = (target - edge).abs();
            if dist <= threshold && best.is_none_or(|(bd, _)| dist < bd) {
                best = Some((dist, target - edge));
            }
        }
    }
    best.map(|(_, adjust)| adjust)
}

/// The candidate scale closest to `scale`, or `scale` itself when none is near
/// enough. Each candidate is paired with the extent it acts over, so distance
/// is measured as how far the picture's edge would move rather than as a change
/// in the factor — 5% is a wide gap on a full-frame clip and nothing at all on
/// a thumbnail.
fn nearest_scale(scale: f32, candidates: &[(f32, f32)], threshold: f32) -> f32 {
    let mut best: Option<(f32, f32)> = None;
    for &(candidate, extent) in candidates {
        if !candidate.is_finite() || candidate <= 0.0 {
            continue;
        }
        let moved = ((candidate - scale) * extent * 0.5).abs();
        if moved <= threshold && best.is_none_or(|(m, _)| moved < m) {
            best = Some((moved, candidate));
        }
    }
    best.map_or(scale, |(_, candidate)| candidate)
}

impl State {
    pub(crate) fn edit_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            timeline: self.timeline.snapshot(),
            pool_order: self.media.ids().to_vec(),
            titles: self.media.title_snapshot(),
            compounds: self.media.compound_snapshot(),
        }
    }

    /// Open an undoable edit. Nests: only the outermost begin/commit pair
    /// yields a step, so a batch (multi-file import) can wrap operations that
    /// each manage their own edit. Must be paired with `commit_edit`.
    pub(crate) fn begin_edit(&mut self) {
        if self.edit_depth == 0 {
            // A visit to the colour picker is a step of its own, ahead of
            // whatever comes next rather than folded into it.
            self.close_color_picker();
            self.pending_edit = Some(self.edit_snapshot());
        }
        self.edit_depth += 1;
    }

    /// Close the edit opened by `begin_edit`. Edits that changed nothing —
    /// a click that never dragged, a split landing on a clip boundary — are
    /// dropped so Ctrl+Z never appears to do nothing.
    pub(crate) fn commit_edit(&mut self) {
        self.edit_depth = self.edit_depth.saturating_sub(1);
        if self.edit_depth > 0 {
            return;
        }
        let Some(before) = self.pending_edit.take() else {
            return;
        };
        self.push_step(before);
    }

    /// Make `before` an undo step, unless nothing has changed since it.
    pub(crate) fn push_step(&mut self, before: EditSnapshot) {
        if before == self.edit_snapshot() {
            return;
        }
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(before);
        // A fresh edit invalidates the redo branch.
        self.redo_stack.clear();
        // Only reached when the edit actually changed something, which is
        // exactly when the saved file goes stale.
        self.dirty = true;
    }

    pub(crate) fn undo(&mut self) {
        self.close_color_picker();
        let Some(prev) = self.undo_stack.pop() else {
            return;
        };
        let current = self.edit_snapshot();
        self.apply_snapshot(&prev);
        self.redo_stack.push(current);
        // Undoing back to exactly the saved state still counts as dirty. The
        // alternative is comparing against a snapshot taken at save time, which
        // is more bookkeeping than an over-eager prompt is worth.
        self.dirty = true;
    }

    pub(crate) fn redo(&mut self) {
        self.close_color_picker();
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        let current = self.edit_snapshot();
        self.apply_snapshot(&next);
        self.undo_stack.push(current);
        self.dirty = true;
    }

    fn apply_snapshot(&mut self, snap: &EditSnapshot) {
        // An in-flight drag holds (track, idx) coordinates that the restored
        // timeline may no longer have — cancel it rather than index a clip
        // that undo just deleted. Dropping the pending snapshot with it means
        // the interrupted drag leaves no half-finished step behind.
        self.drag = DragMode::None;
        self.pending_edit = None;
        self.edit_depth = 0;
        self.timeline.restore(&snap.timeline);
        self.media.set_order(&snap.pool_order);
        self.media.restore_titles(&snap.titles);
        self.media.restore_compounds(&snap.compounds);
        // A step that predates this title, or removes it, leaves nothing to
        // type into.
        self.editing_title = None;
        self.typing_before = None;
        // Recomputed from the timeline on the next render.
        self.last_playing_source = None;
        // Undoing a drop or a trim can shorten the timeline out from under
        // the playhead; pull it back in bounds.
        let duration = self.timeline.duration();
        if self.audio.position() > duration {
            self.audio.set_position(duration);
        }
    }

    pub(crate) fn remove_source(&mut self, id: SourceId) {
        // Closed before the removal rather than after: the edit it opened has
        // to be committed on its own, or the row leaving the pool would land
        // inside the typing session and undo would take back both at once.
        if self.editing_title.as_ref().is_some_and(|(s, _)| *s == id) {
            self.finish_title_edit();
        }
        // A compound clip open for editing, or one around it, cannot leave
        // the pool from inside itself.
        if self.compound_stack.iter().any(|e| self.media.contains_compound(e.source, id) || e.source == id) {
            self.set_status("Close the compound clip before removing it".into(), STATUS_INFO);
            return;
        }
        self.begin_edit();
        self.media.remove(id);
        self.timeline.remove_source(id);
        self.media.remove_from_compounds(id);
        if self.last_playing_source == Some(id) {
            self.last_playing_source = None;
        }
        self.commit_edit();
    }

    /// Double-click on a clip: open it for editing. A compound clip opens to
    /// its contents; any other clip is selected with the effects tab showing
    /// its settings — and a text clip has its text box taking typing, so the
    /// words can be changed straight away.
    pub(crate) fn open_clip(&mut self, track: usize, idx: usize) {
        let source = self.timeline.tracks[track].clips[idx].source;
        if self.media.compound(source).is_some() {
            self.open_compound(source);
            return;
        }
        self.select_clip_at(track, idx);
        if self.pool_tab != PoolTab::Effects {
            self.pool_tab = PoolTab::Effects;
            // Saved with the project's layout, as a click on the tab is.
            self.dirty = true;
        }
        if self.media.title(source).is_some() {
            self.begin_title_edit(source);
        }
    }

    /// Select every clip on the timeline.
    pub(crate) fn select_all(&mut self) {
        self.selected = self.timeline.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.id)).collect();
    }

    /// Make `(track, idx)` the whole selection.
    pub(crate) fn select_clip_at(&mut self, track: usize, idx: usize) {
        self.selected = vec![self.timeline.tracks[track].clips[idx].id];
    }

    /// Add `(track, idx)` to the selection, or take it out if it is already
    /// in: Shift+click. An added clip becomes the primary one.
    pub(crate) fn toggle_clip_at(&mut self, track: usize, idx: usize) {
        let id = self.timeline.tracks[track].clips[idx].id;
        if self.selected.contains(&id) {
            self.selected.retain(|s| *s != id);
        } else {
            self.selected.push(id);
        }
    }

    pub(crate) fn is_selected(&self, id: u32) -> bool {
        self.selected.contains(&id)
    }

    /// Where the primary selected clip is — the one picked last that still
    /// exists.
    pub(crate) fn primary_selected(&self) -> Option<(usize, usize)> {
        self.selected.iter().rev().find_map(|id| self.timeline.find(*id))
    }

    /// Ids of every selected clip and every clip linked to one: what an action
    /// on the selection acts on, since linked audio and video travel as a unit.
    pub(crate) fn selection_with_links(&self) -> std::collections::HashSet<u32> {
        self.selected
            .iter()
            .filter_map(|id| self.timeline.find(*id))
            .flat_map(|(track, idx)| self.linked_siblings(track, idx))
            .map(|(track, idx)| self.timeline.tracks[track].clips[idx].id)
            .collect()
    }

    /// Indices of every clip linked to `(track, idx)`, including itself.
    /// Unlinked clips return just their own position.
    pub(crate) fn linked_siblings(&self, track: usize, idx: usize) -> Vec<(usize, usize)> {
        let link = self.timeline.tracks[track].clips[idx].link;
        let Some(link_id) = link else {
            return vec![(track, idx)];
        };
        let mut v = Vec::new();
        for (ti, tr) in self.timeline.tracks.iter().enumerate() {
            for (ci, c) in tr.clips.iter().enumerate() {
                if c.link == Some(link_id) {
                    v.push((ti, ci));
                }
            }
        }
        v
    }

    /// Times a dragged edge can latch onto: every other clip's edges, the
    /// timeline start, and the playhead. Targets are collected across all
    /// tracks so a clip can be lined up with one on another lane — that's how
    /// you align a cutaway to an edit below it.
    ///
    /// `exclude` is the moving group. A linked pair travels as a unit, so
    /// leaving its own edges in the target set would pin it in place.
    fn snap_targets(&self, exclude: &[(usize, usize)]) -> Vec<f64> {
        let mut pts = vec![0.0, self.audio.position()];
        for (ti, tr) in self.timeline.tracks.iter().enumerate() {
            for (ci, c) in tr.clips.iter().enumerate() {
                if exclude.contains(&(ti, ci)) {
                    continue;
                }
                pts.push(c.timeline_start);
                pts.push(c.timeline_end());
            }
        }
        pts
    }

    /// Nudge `delta` so the closest dragged edge lands exactly on a snap
    /// target. Returns `delta` untouched when nothing is in range or snapping
    /// is off.
    fn snap_move_delta(&self, siblings: &[(usize, usize)], delta: f64) -> f64 {
        let Some(threshold) = self.timeline_snap_threshold() else {
            return delta;
        };
        let edges: Vec<f64> = siblings
            .iter()
            .flat_map(|&(ti, ci)| {
                let c = &self.timeline.tracks[ti].clips[ci];
                [c.timeline_start + delta, c.timeline_end() + delta]
            })
            .collect();
        let targets = self.snap_targets(siblings);
        delta + nearest_snap(&edges, &targets, threshold).unwrap_or(0.0)
    }

    /// How far, in seconds, an edge on the timeline is pulled onto a snap
    /// target. `None` when snapping is off or the timeline has no width.
    ///
    /// The threshold is authored in pixels, so it converts with the same
    /// seconds-per-pixel mapping the drag itself uses — which means the pull
    /// tightens as you zoom in, exactly as the pixel threshold promises.
    fn timeline_snap_threshold(&self) -> Option<f64> {
        if !self.snap_enabled {
            return None;
        }
        let px_per_sec = self.timeline_layout().px_per_sec();
        (px_per_sec.is_finite() && px_per_sec > 0.0).then(|| SNAP_PX as f64 / px_per_sec)
    }

    /// Nudge a trim's `delta` so the edge being dragged lands exactly on a
    /// snap target, as a moved clip's edges do. `start` says which edge: the
    /// left trim moves starts, the right trim ends. Every linked sibling's edge
    /// competes, since the same delta moves them all.
    fn snap_trim_delta(&self, siblings: &[(usize, usize)], delta: f64, start: bool) -> f64 {
        let Some(threshold) = self.timeline_snap_threshold() else {
            return delta;
        };
        let edges: Vec<f64> = siblings
            .iter()
            .map(|&(ti, ci)| {
                let c = &self.timeline.tracks[ti].clips[ci];
                delta + if start { c.timeline_start } else { c.timeline_end() }
            })
            .collect();
        let targets = self.snap_targets(siblings);
        delta + nearest_snap(&edges, &targets, threshold).unwrap_or(0.0)
    }

    /// Where a clip dragged in from the pool starts if it is dropped now: under
    /// the cursor, latched by either of its edges onto the same targets a clip
    /// being moved latches onto. The ghost and the drop both ask this, so the
    /// preview is where the clip lands.
    ///
    /// The paired audio's end counts as an edge too when it differs from the
    /// picture's, since that clip lands alongside and lining it up is as much
    /// the point.
    pub(crate) fn pool_drop_start(&self, source: SourceId) -> f64 {
        let t = self.timeline_layout().cursor_to_t(self.cursor[0]).max(0.0);
        let Some(threshold) = self.timeline_snap_threshold() else {
            return t;
        };
        let mut edges = vec![t, t + self.media.drop_duration(source)];
        if let Some(adur) = self.media.audio_duration(source) {
            edges.push(t + adur);
        }
        let targets = self.snap_targets(&[]);
        let snap = nearest_snap(&edges, &targets, threshold).unwrap_or(0.0);
        (t + snap).max(0.0)
    }

    pub(crate) fn apply_move_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        self.apply_group_move_delta(&siblings, desired_delta);
    }

    /// Move every clip in `siblings` by the same `desired_delta`, snapped and
    /// held off the timeline's start as one: a linked pair, or a selection
    /// dragged together.
    pub(crate) fn apply_group_move_delta(&mut self, siblings: &[(usize, usize)], desired_delta: f64) {
        let siblings = siblings.to_vec();
        // Clamp so the earliest-starting sibling doesn't go negative —
        // applying the same delta everywhere preserves the sync offset.
        let min_start = siblings
            .iter()
            .map(|&(ti, ci)| self.timeline.tracks[ti].clips[ci].timeline_start)
            .fold(f64::INFINITY, f64::min);
        // Snap after the zero-clamp so the latch is computed against where the
        // clip can actually go, then re-clamp as a backstop.
        let delta = self
            .snap_move_delta(&siblings, desired_delta.max(-min_start))
            .max(-min_start);
        for (ti, ci) in siblings {
            self.timeline.tracks[ti].clips[ci].timeline_start += delta;
        }
    }

    pub(crate) fn apply_trim_left_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        // Delta bounds: the same delta shifts every sibling's source_in and
        // timeline_start, so the allowed range is the intersection of each
        // sibling's own limits.
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            min_delta = min_delta.max(-c.source_in);
            min_delta = min_delta.max(-c.timeline_start);
            max_delta = max_delta.min(c.duration() - MIN_CLIP_DURATION);
        }
        // Snapped first, then held to the limits: a target past the start of
        // the source or through the clip's own end is not one it can reach.
        let delta = self
            .snap_trim_delta(&siblings, desired_delta, true)
            .clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_in += delta;
            c.timeline_start += delta;
            c.clamp_fades();
        }
    }

    pub(crate) fn apply_trim_right_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            // Can't shrink below the minimum clip duration.
            min_delta = min_delta.max(MIN_CLIP_DURATION - c.duration());
            // Can't extend past the source's end — cap per-track since video
            // and audio streams of the same source can have different lengths.
            let src_dur = match self.timeline.tracks[ti].kind {
                TrackKind::Video => self.media.duration(c.source),
                TrackKind::Audio => self.media.audio_duration(c.source).unwrap_or(c.source_out),
            };
            max_delta = max_delta.min(src_dur - c.source_out);
        }
        let delta = self
            .snap_trim_delta(&siblings, desired_delta, false)
            .clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_out += delta;
            c.clamp_fades();
        }
    }

    /// Set a clip's level.
    ///
    /// Deliberately not applied to linked siblings, unlike move and trim. A
    /// link says two clips share a position in time, not a level: the video
    /// half of a pair has no audio to turn down, and its opacity is a different
    /// quantity that happens to share a slider shape.
    pub(crate) fn set_clip_gain(&mut self, track: usize, idx: usize, gain: f32) {
        let clip = &mut self.timeline.tracks[track].clips[idx];
        clip.gain = gain.clamp(db_to_gain(MIN_GAIN_DB), db_to_gain(MAX_GAIN_DB));
    }

    /// Set a video clip's opacity — the picture's counterpart to
    /// [`State::set_clip_gain`], and left off linked siblings for the same
    /// reason.
    pub(crate) fn set_clip_opacity(&mut self, track: usize, idx: usize, opacity: f32) {
        let clip = &mut self.timeline.tracks[track].clips[idx];
        clip.opacity = opacity.clamp(MIN_OPACITY, MAX_OPACITY);
    }

    /// Replace a clip's transform, clamped to what can be rendered.
    ///
    /// The scale bound is not cosmetic: it is what caps the intermediate the
    /// export scales a placed clip through, so a stray drag cannot ask the
    /// renderer for a picture the size of a wall.
    pub(crate) fn set_clip_transform(&mut self, track: usize, idx: usize, transform: Transform) {
        let clip = &mut self.timeline.tracks[track].clips[idx];
        clip.transform = Transform {
            scale: transform.scale.clamp(MIN_SCALE, MAX_SCALE),
            ..transform
        };
    }

    /// How wide the canvas magnet's pull is, in canvas pixels.
    ///
    /// Authored in points and converted through the preview's own scale, the
    /// same bargain the timeline's threshold makes with its zoom: the pull is
    /// the distance the hand moved, so it stays the same size under the cursor
    /// whatever the panel has been resized to. `None` when snapping is off or
    /// the preview has no size to speak of.
    fn canvas_snap_threshold(&self) -> Option<f32> {
        if !self.snap_enabled {
            return None;
        }
        // `is_finite` first: a zero-sized window can put a NaN in the scale,
        // and a NaN compares false against every bound including this one.
        let scale = self.preview_canvas_scale;
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        Some(SNAP_PX / scale)
    }

    /// Latch a dragged placement onto the frame: each edge to its own edge of
    /// the canvas, and each centre line to the canvas's.
    ///
    /// The same magnet the timeline clips use, under the same toggle — lining
    /// a picture up with the frame is the same act as lining a clip up with its
    /// neighbour, and a second control for it would be a second thing to find.
    ///
    /// Both axes latch independently, so an overlay can sit flush against the
    /// left edge while still floating free vertically.
    pub(crate) fn snap_transform_offset(
        &self,
        source_size: (u32, u32),
        transform: Transform,
    ) -> Transform {
        let Some(threshold) = self.canvas_snap_threshold() else {
            return transform;
        };
        let canvas = self.canvas();
        let (sw, sh) = (source_size.0 as f32, source_size.1 as f32);
        let [x, y, w, h] = canvas.place(sw, sh, transform);
        let (cw, ch) = (canvas.width as f32, canvas.height as f32);
        // Expressed in canvas pixels, then converted back to the fraction the
        // transform stores, so the arithmetic happens in the units the targets
        // are named in.
        let dx = nearest_snap(
            &[x as f64, (x + w * 0.5) as f64, (x + w) as f64],
            &[0.0, cw as f64 * 0.5, cw as f64],
            threshold as f64,
        )
        .unwrap_or(0.0) as f32;
        let dy = nearest_snap(
            &[y as f64, (y + h * 0.5) as f64, (y + h) as f64],
            &[0.0, ch as f64 * 0.5, ch as f64],
            threshold as f64,
        )
        .unwrap_or(0.0) as f32;
        Transform {
            x: transform.x + dx / cw,
            y: transform.y + dy / ch,
            ..transform
        }
    }

    /// The transform that scales a picture to `scale` while holding the corner
    /// opposite `corner` exactly where it is.
    ///
    /// What a corner handle has always meant: the edges under the hand move and
    /// the ones across from them do not. Scaling about the centre instead would
    /// walk a picture out from under a corner the user was lining up.
    ///
    /// `anchor` is that fixed corner in canvas pixels, taken once when the drag
    /// began — recomputing it each frame from the current rect would let
    /// rounding creep it along over a long drag.
    pub(crate) fn transform_scaled_about(
        &self,
        source_size: (u32, u32),
        anchor: [f32; 2],
        corner: [f32; 2],
        scale: f32,
    ) -> Transform {
        let canvas = self.canvas();
        let (fx, fy, fw, fh) = canvas.fit(source_size.0 as f32, source_size.1 as f32);
        let scale = scale.clamp(MIN_SCALE, MAX_SCALE);
        let (w, h) = (fw * scale, fh * scale);
        // The anchor is the far corner, so it sits `1 - corner` of the way
        // across the new rect; that fixes where the rect starts, and inverting
        // `Canvas::place` turns that back into the offset the clip stores.
        let x = anchor[0] - (1.0 - corner[0]) * w;
        let y = anchor[1] - (1.0 - corner[1]) * h;
        Transform {
            x: (x - fx - (fw - w) * 0.5) / canvas.width as f32,
            y: (y - fy - (fh - h) * 0.5) / canvas.height as f32,
            scale,
        }
    }

    /// Latch a corner drag onto the sizes worth landing on: filling the frame,
    /// and either moving edge flush with the canvas edge it is nearest.
    ///
    /// Solved as "which scale would put that edge there" rather than nudged
    /// after the fact, because the scale is what the drag controls — correcting
    /// the edge directly would leave the two out of step.
    pub(crate) fn snap_scale_about(
        &self,
        source_size: (u32, u32),
        anchor: [f32; 2],
        corner: [f32; 2],
        scale: f32,
    ) -> f32 {
        let Some(threshold) = self.canvas_snap_threshold() else {
            return scale;
        };
        let canvas = self.canvas();
        let (_, _, fw, fh) = canvas.fit(source_size.0 as f32, source_size.1 as f32);
        if fw <= 0.0 || fh <= 0.0 {
            return scale;
        }
        // With the far corner pinned, the moving edge of each axis sits at
        // `anchor + (2 * corner - 1) * extent * scale` — one step away when the
        // handle is bottom-right, the other way when it is top-left.
        let along_x = (2.0 * corner[0] - 1.0) * fw;
        let along_y = (2.0 * corner[1] - 1.0) * fh;
        let candidates = [
            // Back to the plain fit — the size a clip arrives at, and the one
            // worth being able to get back to exactly.
            (1.0, fw),
            (-anchor[0] / along_x, fw),
            ((canvas.width as f32 - anchor[0]) / along_x, fw),
            (-anchor[1] / along_y, fh),
            ((canvas.height as f32 - anchor[1]) / along_y, fh),
        ];
        nearest_scale(scale, &candidates, threshold)
    }

    /// Set one of a clip's fades, in seconds. Clamped into the clip the same
    /// way a trim's is — see [`crate::timeline::Clip::clamp_fades`].
    pub(crate) fn set_clip_fade(&mut self, track: usize, idx: usize, side: FadeSide, len: f64) {
        let clip = &mut self.timeline.tracks[track].clips[idx];
        let len = len.max(0.0);
        match side {
            FadeSide::In => clip.fade_in = len.min(clip.duration() - clip.fade_out),
            FadeSide::Out => clip.fade_out = len.min(clip.duration() - clip.fade_in),
        }
        clip.clamp_fades();
    }

    /// Nudge the selected clip's level one step, for the keyboard: `dir` is +1
    /// for up and -1 for down, and `fine` asks for the smaller step.
    ///
    /// Which quantity moves follows the clip's track, the same way the level
    /// line under the cursor does. Audio steps in decibels rather than in the
    /// stored linear gain, so one press is the same perceived step wherever the
    /// level already sits; opacity is already linear and steps in percent.
    ///
    /// Every selected clip moves by the same step from its own level, as one
    /// undo step.
    pub(crate) fn nudge_selected_level(&mut self, dir: f32, fine: bool) {
        let targets: Vec<(usize, usize)> =
            self.selected.iter().filter_map(|id| self.timeline.find(*id)).collect();
        if targets.is_empty() {
            return;
        }
        self.begin_edit();
        for (track, idx) in targets {
            let clip = self.timeline.tracks[track].clips[idx];
            match self.timeline.tracks[track].kind {
                TrackKind::Audio => {
                    let step = if fine { GAIN_STEP_FINE_DB } else { GAIN_STEP_DB };
                    let db = gain_to_db(clip.gain) + dir * step;
                    self.set_clip_gain(track, idx, db_to_gain(db));
                }
                TrackKind::Video => {
                    let step = if fine { OPACITY_STEP_FINE } else { OPACITY_STEP };
                    self.set_clip_opacity(track, idx, clip.opacity + dir * step);
                }
            }
        }
        self.commit_edit();
    }

    /// Add a title to the pool and start typing into it.
    ///
    /// Straight into editing because a title that says "Title" is not a title
    /// yet — and because the one gesture that teaches how to edit one is the
    /// one that puts the first one on screen.
    pub(crate) fn add_title(&mut self) {
        self.begin_edit();
        let id = self.media.add_title(Title::default());
        self.commit_edit();
        self.begin_title_edit(id);
    }

    /// Start typing into `source`'s title, if it has one.
    pub(crate) fn begin_title_edit(&mut self, source: SourceId) {
        let Some(title) = self.media.title(source).cloned() else {
            return;
        };
        self.finish_title_edit();
        self.close_color_picker();
        self.title_caret = title.text.len();
        self.editing_title = Some((source, title));
        // One typing session is one undo step, however many keys it took —
        // the same bargain a drag makes. Kept apart from the drag's own edit
        // rather than nested in it: typing usually starts with a press, and
        // the release that ends the press must not end the typing too.
        self.typing_before = Some(self.edit_snapshot());
    }

    /// Start typing into the selected clip's title, for the keyboard. Does
    /// nothing when the selection is footage, which has no text to change.
    pub(crate) fn edit_selected_title(&mut self) {
        let Some((track, idx)) = self.primary_selected() else {
            return;
        };
        let source = self.timeline.tracks[track].clips[idx].source;
        self.begin_title_edit(source);
    }

    pub(crate) fn typing_title(&self) -> bool {
        self.editing_title.is_some()
    }

    /// Accept the text as it stands.
    pub(crate) fn finish_title_edit(&mut self) {
        if self.editing_title.take().is_some() {
            if let Some(before) = self.typing_before.take() {
                self.push_step(before);
            }
        }
    }

    /// The title being typed into, its text, and the caret held inside it —
    /// an undo or another edit can have shortened the text under the caret.
    fn typing_text(&mut self) -> Option<(SourceId, String)> {
        let (source, _) = self.editing_title.as_ref()?;
        let source = *source;
        let text = self.media.title(source)?.text.clone();
        let mut i = self.title_caret.min(text.len());
        while !text.is_char_boundary(i) {
            i -= 1;
        }
        self.title_caret = i;
        Some((source, text))
    }

    fn set_typed_text(&mut self, source: SourceId, text: String) {
        if let Some(mut title) = self.media.title(source).cloned() {
            title.text = text;
            self.media.set_title(source, title);
        }
    }

    /// Put typed text in at the caret, and the caret after it.
    ///
    /// Control characters are dropped: the window hands over the key's text
    /// whatever it was, and a tab or an escape has no business in a title even
    /// though the keyboard reports one.
    pub(crate) fn type_into_title(&mut self, typed: &str) {
        let Some((source, mut text)) = self.typing_text() else {
            return;
        };
        let typed: String = typed.chars().filter(|c| !c.is_control() || *c == '\n').collect();
        if typed.is_empty() {
            return;
        }
        text.insert_str(self.title_caret, &typed);
        self.title_caret += typed.len();
        self.set_typed_text(source, text);
    }

    /// Remove the character before the caret.
    pub(crate) fn backspace_title(&mut self) {
        let Some((source, mut text)) = self.typing_text() else {
            return;
        };
        let from = title::caret::left(&text, self.title_caret);
        if from < self.title_caret {
            text.replace_range(from..self.title_caret, "");
            self.title_caret = from;
            self.set_typed_text(source, text);
        }
    }

    /// Remove the character after the caret.
    pub(crate) fn delete_in_title(&mut self) {
        let Some((source, mut text)) = self.typing_text() else {
            return;
        };
        let to = title::caret::right(&text, self.title_caret);
        if to > self.title_caret {
            text.replace_range(self.title_caret..to, "");
            self.set_typed_text(source, text);
        }
    }

    /// Move the caret with one of [`title::caret`]'s moves.
    pub(crate) fn move_title_caret(&mut self, step: fn(&str, usize) -> usize) {
        if let Some((_, text)) = self.typing_text() {
            self.title_caret = step(&text, self.title_caret);
        }
    }

    /// Copy the selected clips, and the clips linked to them, for a paste.
    pub(crate) fn copy_selected(&mut self) {
        let ids = self.selection_with_links();
        if ids.is_empty() {
            self.set_status("Select clips to copy".into(), STATUS_INFO);
            return;
        }
        self.clipboard = self.timeline.copy(&ids);
        let n = self.clipboard.clips.len();
        let noun = if n == 1 { "clip" } else { "clips" };
        self.set_status(format!("Copied {n} {noun}"), STATUS_INFO);
    }

    /// Paste what was copied at the playhead, or just after the clip the
    /// playhead is on, and select it. The playhead moves to the end of what
    /// was pasted, so pressing paste again lays the next copy after it.
    ///
    /// A title comes back as a title of its own, a copy of the words: changing
    /// one afterwards leaves the other as it was, which is what copying text
    /// means everywhere else.
    pub(crate) fn paste(&mut self) {
        let mut board = self.clipboard.clone();
        // A source taken out of the pool since the copy has nothing to show.
        board.clips.retain(|(_, _, c)| self.media.get(c.source).is_some());
        // Nor can a compound clip go inside itself.
        let before = board.clips.len();
        board.clips.retain(|(_, _, c)| self.may_place(c.source));
        if board.clips.len() < before {
            self.refuse_nesting();
        }
        if board.is_empty() {
            self.set_status("Nothing to paste - copy clips first".into(), STATUS_INFO);
            return;
        }
        self.begin_edit();
        let mut fresh = std::collections::HashMap::new();
        for (_, _, clip) in &mut board.clips {
            if let Some(title) = self.media.title(clip.source).cloned() {
                clip.source =
                    *fresh.entry(clip.source).or_insert_with(|| self.media.add_title(title));
            }
        }
        let ids = self.timeline.paste(&board, self.audio.position());
        self.commit_edit();
        let end = ids
            .iter()
            .filter_map(|id| self.timeline.find(*id))
            .map(|(t, i)| self.timeline.tracks[t].clips[i].timeline_end())
            .fold(0.0, f64::max);
        self.selected = ids;
        self.audio.set_position(end);
    }

    /// Cut the selected clips, and the clips linked to them, at the playhead.
    /// Nothing else on the timeline is touched: a split is a decision about a
    /// clip, and cutting every track under the playhead made one about clips
    /// nobody had picked.
    pub(crate) fn split_at_playhead(&mut self) {
        let ids = self.selection_with_links();
        if ids.is_empty() {
            self.set_status("Select a clip to split".into(), STATUS_INFO);
            return;
        }
        let t = self.audio.position();
        self.begin_edit();
        self.timeline.split_at(t, |clip| ids.contains(&clip.id));
        self.commit_edit();
    }

    /// Whether [`State::delete_selected`] would actually remove anything.
    pub(crate) fn has_selection(&self) -> bool {
        self.selected.iter().any(|id| self.timeline.find(*id).is_some())
    }

    /// Remove the selected clips, taking their linked siblings with them.
    /// Linked A/V travels as a unit everywhere else — move, trim, split — so
    /// deleting only half of a pair would be the odd one out.
    pub(crate) fn delete_selected(&mut self) {
        let doomed = self.selection_with_links();
        if doomed.is_empty() {
            return;
        }
        self.begin_edit();
        for t in &mut self.timeline.tracks {
            t.clips.retain(|c| !doomed.contains(&c.id));
        }
        self.commit_edit();
        self.selected.clear();
    }

    /// A new empty track of `kind`: above the other video tracks, or below
    /// the other audio ones — the edge of the stack its add row sits on.
    pub(crate) fn add_track(&mut self, kind: TrackKind) {
        self.begin_edit();
        self.timeline.push_track(kind);
        self.commit_edit();
    }

    /// Take out track `idx`, which the header only offers for an empty track
    /// that is not the last of its kind — checked again here, since a track
    /// with clips on it going is a loss nobody asked for.
    pub(crate) fn remove_track(&mut self, idx: usize) {
        let Some(track) = self.timeline.tracks.get(idx) else {
            return;
        };
        if !track.clips.is_empty() || self.n_tracks_of(track.kind) <= 1 {
            return;
        }
        self.begin_edit();
        self.timeline.tracks.remove(idx);
        self.commit_edit();
    }

    /// Put a fresh `kind` on the end of each clip at `targets`, as one step.
    /// A clip already carrying all it can hold is skipped and said so.
    pub(crate) fn apply_effect(&mut self, kind: EffectKind, targets: &[(usize, usize)]) {
        self.begin_edit();
        let mut full = 0;
        for &(track, idx) in targets {
            if !self.timeline.tracks[track].clips[idx].effects.push(kind.instance()) {
                full += 1;
            }
        }
        self.commit_edit();
        if full > 0 {
            self.set_status(
                format!("A clip can hold {MAX_EFFECTS} effects; {full} already had that many"),
                STATUS_INFO,
            );
        }
    }

    /// The apply button: `kind` on every selected clip's picture — each
    /// selected video clip, and the video half of each selected audio clip.
    pub(crate) fn apply_effect_to_selection(&mut self, kind: EffectKind) {
        let mut targets: Vec<(usize, usize)> = Vec::new();
        for id in &self.selected {
            let Some((track, idx)) = self.timeline.find(*id) else {
                continue;
            };
            if let Some(target) = self.effects_clip_for(track, idx) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if targets.is_empty() {
            self.set_status("Select a video clip to apply an effect to".into(), STATUS_INFO);
            return;
        }
        self.apply_effect(kind, &targets);
    }

    /// Effect `index` on the clip the effects tab is showing, for changing.
    fn shown_effect_mut(&mut self, index: usize) -> Option<&mut Effect> {
        let (track, idx) = self.effects_target()?;
        self.timeline.tracks[track].clips[idx].effects.get_mut(index)
    }

    pub(crate) fn toggle_effect(&mut self, index: usize) {
        self.begin_edit();
        if let Some(effect) = self.shown_effect_mut(index) {
            effect.enabled = !effect.enabled;
        }
        self.commit_edit();
    }

    pub(crate) fn remove_effect(&mut self, index: usize) {
        self.begin_edit();
        if let Some((track, idx)) = self.effects_target() {
            self.timeline.tracks[track].clips[idx].effects.remove(index);
        }
        self.commit_edit();
    }

    /// Set one parameter of the shown clip's effect `index`. Called through a
    /// slider drag, which holds the edit open around it.
    /// Change the title the effects tab is showing, as part of an edit
    /// already open — a slider drag, which is one step however far it goes.
    pub(crate) fn edit_shown_title(&mut self, change: impl FnOnce(&mut Title)) {
        let Some((source, title)) = self.effects_title() else {
            return;
        };
        let mut title = title.clone();
        change(&mut title);
        self.media.set_title(source, title);
    }

    /// Change the title the effects tab is showing, as one undo step.
    pub(crate) fn change_shown_title(&mut self, change: impl FnOnce(&mut Title)) {
        self.begin_edit();
        self.edit_shown_title(change);
        self.commit_edit();
    }

    /// Drop the font list from the font box, scrolled so the font in use is
    /// in view.
    pub(crate) fn open_font_menu(&mut self) {
        let Some((_, title)) = self.effects_title() else {
            return;
        };
        let current = title
            .font
            .as_deref()
            .and_then(|f| fonts::families().iter().position(|n| n == f))
            .map_or(0, |i| i + 1);
        self.font_menu = Some((TitleMenu::Font, current.saturating_sub(FONT_MENU_ROWS / 2)));
    }

    /// Drop the list of the weights the shown title's font comes in.
    pub(crate) fn open_weight_menu(&mut self) {
        if self.effects_title().is_some() {
            self.font_menu = Some((TitleMenu::Weight, 0));
        }
    }

    pub(crate) fn set_effect_param(&mut self, index: usize, param: usize, value: f32) {
        if let Some(effect) = self.shown_effect_mut(index) {
            effect.set_param(param, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snap offsets are differences of decimal literals, so they carry the
    /// usual float dust; compare within a tolerance far tighter than any
    /// audible or visible difference.
    fn assert_close(got: Option<f64>, want: f64) {
        let got = got.expect("expected a snap");
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    #[test]
    fn snaps_the_trailing_edge_flush_against_a_neighbour() {
        // Clip [0,10) dragged so its end sits at 9.7; a neighbour starts at
        // 10. The gap closes exactly, leaving no overlap and no seam.
        assert_close(nearest_snap(&[-0.3, 9.7], &[10.0, 15.0], 0.5), 0.3);
    }

    #[test]
    fn snaps_by_whichever_edge_is_closest() {
        // Leading edge is 0.1 from a target, trailing edge 0.4 from another.
        // The leading edge wins even though both are in range.
        assert_close(nearest_snap(&[4.9, 14.6], &[5.0, 15.0], 0.5), 0.1);
    }

    #[test]
    fn nothing_latches_outside_the_threshold() {
        assert_eq!(nearest_snap(&[4.0], &[5.0], 0.5), None);
    }

    #[test]
    fn snapping_pulls_backwards_too() {
        // Overlapping a neighbour by 0.2 pulls back out to flush, so a drag
        // that overshoots still lands clean.
        assert_close(nearest_snap(&[10.2], &[10.0], 0.5), -0.2);
    }

    #[test]
    fn no_targets_means_no_snap() {
        assert_eq!(nearest_snap(&[4.0, 9.0], &[], 0.5), None);
    }

    /// Dragged to within a few pixels of filling the frame, a clip lands on it
    /// exactly — the whole point of the magnet on the canvas.
    #[test]
    fn a_scale_near_a_landmark_latches_onto_it() {
        // 1920 wide: the tentative scale puts the edge 9.6px off full frame.
        assert_eq!(nearest_scale(0.99, &[(1.0, 1920.0)], 12.0), 1.0);
    }

    /// The threshold is a distance on the canvas, so the same proportional
    /// error on a small picture is well inside it and on a large one is not.
    #[test]
    fn the_pull_is_measured_where_the_edge_would_move() {
        assert_eq!(nearest_scale(0.9, &[(1.0, 1920.0)], 12.0), 0.9);
        assert_eq!(nearest_scale(0.9, &[(1.0, 200.0)], 12.0), 1.0);
    }

    /// Two landmarks in range, and the nearer one wins — otherwise which edge
    /// a picture latches to would depend on the order they were listed in.
    #[test]
    fn the_nearest_landmark_wins() {
        assert_eq!(nearest_scale(1.0, &[(1.01, 1920.0), (1.002, 1920.0)], 20.0), 1.002);
    }

    /// A landmark behind the picture's own centre solves to a negative or
    /// infinite scale; those are arithmetic, not sizes.
    #[test]
    fn impossible_candidates_are_ignored() {
        assert_eq!(nearest_scale(1.0, &[(-0.5, 1920.0), (f32::INFINITY, 1920.0)], 1e9), 1.0);
    }
}
