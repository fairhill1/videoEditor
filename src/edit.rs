//! Edits to the timeline, and the history that can take them back.
//!
//! Everything here mutates the model and nothing here reads the cursor: the
//! gestures that drive these live in `input.rs` and arrive as already-resolved
//! deltas. That split is what lets a keyboard shortcut and a drag share one
//! implementation of "move this clip".

use crate::input::DragMode;
use crate::state::State;
use crate::theme::SNAP_PX;
use crate::timeline::{SourceId, TimelineSnapshot, TrackKind};

/// Keeps trim from zeroing a clip, in seconds.
pub(crate) const MIN_CLIP_DURATION: f64 = 0.05;

/// Retained undo steps. Snapshots are small, but a long session shouldn't grow
/// without bound; the oldest step is dropped past this.
const UNDO_LIMIT: usize = 200;

/// One undo step: everything a user edit can change. Pool membership rides
/// along with the timeline because deleting a pool item also deletes its
/// clips — undoing that has to put both back in one move.
#[derive(Clone, PartialEq)]
pub(crate) struct EditSnapshot {
    timeline: TimelineSnapshot,
    pool_order: Vec<SourceId>,
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

impl State {
    fn edit_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            timeline: self.timeline.snapshot(),
            pool_order: self.media.ids().to_vec(),
        }
    }

    /// Open an undoable edit. Nests: only the outermost begin/commit pair
    /// yields a step, so a batch (multi-file import) can wrap operations that
    /// each manage their own edit. Must be paired with `commit_edit`.
    pub(crate) fn begin_edit(&mut self) {
        if self.edit_depth == 0 {
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
        self.begin_edit();
        self.media.remove(id);
        self.timeline.remove_source(id);
        if self.last_playing_source == Some(id) {
            self.last_playing_source = None;
        }
        self.commit_edit();
    }

    pub(crate) fn select_clip_at(&mut self, track: usize, idx: usize) {
        self.selected = Some(self.timeline.tracks[track].clips[idx].id);
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
        if !self.snap_enabled {
            return delta;
        }
        let layout = self.timeline_layout();
        // The threshold is authored in pixels, so convert with the same
        // seconds-per-pixel mapping the drag itself uses.
        let px_per_sec = layout.clips_w as f64 / layout.duration;
        if !px_per_sec.is_finite() || px_per_sec <= 0.0 {
            return delta;
        }
        let edges: Vec<f64> = siblings
            .iter()
            .flat_map(|&(ti, ci)| {
                let c = &self.timeline.tracks[ti].clips[ci];
                [c.timeline_start + delta, c.timeline_end() + delta]
            })
            .collect();
        let targets = self.snap_targets(siblings);
        delta + nearest_snap(&edges, &targets, SNAP_PX as f64 / px_per_sec).unwrap_or(0.0)
    }

    pub(crate) fn apply_move_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
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
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_in += delta;
            c.timeline_start += delta;
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
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_out += delta;
        }
    }

    pub(crate) fn split_at_playhead(&mut self) {
        let t = self.audio.position();
        self.begin_edit();
        self.timeline.split_at(t);
        self.commit_edit();
    }

    /// Whether [`State::delete_selected`] would actually remove anything.
    pub(crate) fn has_selection(&self) -> bool {
        self.selected
            .is_some_and(|id| self.timeline.find(id).is_some())
    }

    /// Remove the selected clip, taking its linked siblings with it. Linked
    /// A/V travels as a unit everywhere else — move, trim, split — so deleting
    /// only half of a pair would be the odd one out.
    pub(crate) fn delete_selected(&mut self) {
        let Some((track, idx)) = self.selected.and_then(|id| self.timeline.find(id)) else {
            return;
        };
        // Resolve to ids before touching anything: every removal shifts the
        // indices of the clips after it, so a list of positions goes stale the
        // moment the first one is used.
        let doomed: Vec<u32> = self
            .linked_siblings(track, idx)
            .into_iter()
            .map(|(ti, ci)| self.timeline.tracks[ti].clips[ci].id)
            .collect();
        self.begin_edit();
        for t in &mut self.timeline.tracks {
            t.clips.retain(|c| !doomed.contains(&c.id));
        }
        self.commit_edit();
        self.selected = None;
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
}
