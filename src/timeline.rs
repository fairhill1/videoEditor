/// Slack when testing "is the playhead already on this boundary". Jumping to a
/// boundary lands the playhead on it exactly, but the position round-trips
/// through the audio engine, so an exact compare would let float drift strand
/// the next jump.
const EDIT_POINT_EPS: f64 = 1e-6;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Clip {
    /// Stable name for this clip, unlike its position in `Track::clips`.
    /// Splitting inserts, deleting removes, and dragging to another track moves
    /// a clip between vectors — any of which silently reassigns indices. The
    /// UI holds one of these to keep pointing at the clip you actually picked.
    pub id: u32,
    pub source: SourceId,
    pub source_in: f64,
    pub source_out: f64,
    pub timeline_start: f64,
    /// Clips sharing a link id move, trim, and split together. Assigned when
    /// auto-pairing a video drop with its audio sibling; propagated across
    /// splits so each pair of halves stays linked to the correct counterpart.
    pub link: Option<u32>,
}

impl Clip {
    pub fn duration(&self) -> f64 {
        (self.source_out - self.source_in).max(0.0)
    }

    pub fn timeline_end(&self) -> f64 {
        self.timeline_start + self.duration()
    }

    pub fn contains(&self, t: f64) -> bool {
        t >= self.timeline_start && t < self.timeline_end()
    }

    pub fn source_time(&self, t: f64) -> f64 {
        self.source_in + (t - self.timeline_start).max(0.0)
    }
}

pub struct Track {
    pub kind: TrackKind,
    pub clips: Vec<Clip>,
}

impl Track {
    pub fn new(kind: TrackKind) -> Self {
        Self {
            kind,
            clips: Vec::new(),
        }
    }

    pub fn active_clip(&self, t: f64) -> Option<&Clip> {
        self.clips.iter().find(|c| c.contains(t))
    }
}

pub struct Timeline {
    pub tracks: Vec<Track>,
    next_link: u32,
    next_clip: u32,
}

/// Complete copy of the timeline's mutable state, for undo/redo. Clips are
/// `Copy` and a project holds thousands at most, so snapshotting the whole
/// thing per edit is cheaper — and far less bug-prone — than maintaining an
/// inverse operation for every edit.
#[derive(Clone, Debug, PartialEq)]
pub struct TimelineSnapshot {
    tracks: Vec<(TrackKind, Vec<Clip>)>,
    next_link: u32,
    next_clip: u32,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_link: 0,
            next_clip: 0,
        }
    }

    /// Allocate a fresh clip id. Every `Clip` must get one from here so that
    /// no two clips ever share a name.
    pub fn new_clip_id(&mut self) -> u32 {
        let id = self.next_clip;
        self.next_clip += 1;
        id
    }

    /// Position of the clip called `id`, or `None` once it has been deleted.
    /// A stale id is inert rather than wrong — which is what lets a selection
    /// survive an undo that brings its clip back.
    pub fn find(&self, id: u32) -> Option<(usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(ti, track)| {
            track
                .clips
                .iter()
                .position(|c| c.id == id)
                .map(|ci| (ti, ci))
        })
    }

    /// Allocate a fresh link id. Call this when establishing a new linked
    /// group (e.g. auto-pairing a video drop with its audio clip).
    pub fn new_link_id(&mut self) -> u32 {
        let id = self.next_link;
        self.next_link += 1;
        id
    }

    pub fn snapshot(&self) -> TimelineSnapshot {
        TimelineSnapshot {
            tracks: self
                .tracks
                .iter()
                .map(|t| (t.kind, t.clips.clone()))
                .collect(),
            next_link: self.next_link,
            next_clip: self.next_clip,
        }
    }

    /// Rewinding `next_link` can't collide: every clip holding an id at or
    /// above the restored counter is discarded by the same restore.
    pub fn restore(&mut self, snap: &TimelineSnapshot) {
        self.tracks = snap
            .tracks
            .iter()
            .map(|(kind, clips)| Track {
                kind: *kind,
                clips: clips.clone(),
            })
            .collect();
        self.next_link = snap.next_link;
        self.next_clip = snap.next_clip;
    }

    pub fn remove_source(&mut self, source: SourceId) {
        for track in &mut self.tracks {
            track.clips.retain(|c| c.source != source);
        }
    }

    pub fn duration(&self) -> f64 {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.timeline_end()))
            .fold(0.0_f64, f64::max)
    }

    /// Nearest clip boundary strictly before `t`, or `None` if `t` is at or
    /// before the first one. Boundaries come from every track, so an
    /// audio-only edit is navigable too; 0.0 always counts so the playhead can
    /// get back to the start of a timeline whose first clip begins later.
    ///
    /// "Strictly before" is what makes repeated presses walk backwards instead
    /// of dead-ending on the boundary the playhead already sits on.
    pub fn prev_edit_point(&self, t: f64) -> Option<f64> {
        self.edit_points()
            .into_iter()
            .filter(|&p| p < t - EDIT_POINT_EPS)
            .fold(None, |acc: Option<f64>, p| {
                Some(acc.map_or(p, |a| a.max(p)))
            })
    }

    /// Nearest clip boundary strictly after `t`. See [`Timeline::prev_edit_point`].
    pub fn next_edit_point(&self, t: f64) -> Option<f64> {
        self.edit_points()
            .into_iter()
            .filter(|&p| p > t + EDIT_POINT_EPS)
            .fold(None, |acc: Option<f64>, p| {
                Some(acc.map_or(p, |a| a.min(p)))
            })
    }

    fn edit_points(&self) -> Vec<f64> {
        let mut pts = vec![0.0];
        for track in &self.tracks {
            for clip in &track.clips {
                pts.push(clip.timeline_start);
                pts.push(clip.timeline_end());
            }
        }
        pts
    }

    /// Topmost active video clip at `t`. Higher track index = on top.
    pub fn topmost_video_clip(&self, t: f64) -> Option<(usize, &Clip)> {
        self.tracks
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, tr)| tr.kind == TrackKind::Video)
            .find_map(|(i, tr)| tr.active_clip(t).map(|c| (i, c)))
    }

    /// Split every clip containing `t` into two clips meeting at `t`. Clips whose
    /// start aligns exactly with `t` are left alone — there's nothing to split.
    ///
    /// Link preservation: the left halves keep the original link id; the right
    /// halves get a new shared link id (per old link id) so that e.g. video +
    /// audio clips linked together end up with their right halves linked to
    /// each other. Unlinked clips stay unlinked.
    pub fn split_at(&mut self, t: f64) {
        use std::collections::{HashMap, HashSet};

        // First pass: find every old link id on a clip that will actually split.
        // We do this up front so we can allocate new ids without holding a
        // mutable borrow on `self.tracks` while also bumping `self.next_link`.
        let mut old_links: HashSet<u32> = HashSet::new();
        for track in &self.tracks {
            for clip in &track.clips {
                if clip.contains(t) && t > clip.timeline_start {
                    if let Some(l) = clip.link {
                        old_links.insert(l);
                    }
                }
            }
        }
        let mut relink: HashMap<u32, u32> = HashMap::new();
        for old in old_links {
            let new_id = self.next_link;
            self.next_link += 1;
            relink.insert(old, new_id);
        }

        // Held locally because the loop below borrows `self.tracks` mutably;
        // written back once it releases.
        let mut next_clip = self.next_clip;
        for track in &mut self.tracks {
            let mut i = 0;
            while i < track.clips.len() {
                let orig = track.clips[i];
                if orig.contains(t) && t > orig.timeline_start {
                    let split_source_t = orig.source_time(t);
                    track.clips[i].source_out = split_source_t;
                    let right_link = orig.link.map(|old| relink[&old]);
                    // The left half keeps the original id, so a selection on
                    // the clip you split stays on the part before the cut.
                    let right_id = next_clip;
                    next_clip += 1;
                    track.clips.insert(
                        i + 1,
                        Clip {
                            id: right_id,
                            source: orig.source,
                            source_in: split_source_t,
                            source_out: orig.source_out,
                            timeline_start: t,
                            link: right_link,
                        },
                    );
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
        self.next_clip = next_clip;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids are irrelevant to most of these tests, so they all share one;
    /// `timeline_with` hands out distinct ones where it matters.
    fn clip(start: f64, dur: f64) -> Clip {
        Clip {
            id: 0,
            source: SourceId(0),
            source_in: 0.0,
            source_out: dur,
            timeline_start: start,
            link: None,
        }
    }

    fn timeline_with(clips: Vec<Clip>) -> Timeline {
        let mut tl = Timeline::new();
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks[0].clips = clips;
        for i in 0..tl.tracks[0].clips.len() {
            tl.tracks[0].clips[i].id = tl.new_clip_id();
        }
        tl
    }

    #[test]
    fn restore_undoes_a_split() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        let before = tl.snapshot();

        tl.split_at(4.0);
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_ne!(tl.snapshot(), before);

        tl.restore(&before);
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.snapshot(), before);
    }

    #[test]
    fn restore_rewinds_link_ids_so_a_redone_split_reuses_them() {
        let mut tl = timeline_with(vec![Clip {
            link: Some(0),
            ..clip(0.0, 10.0)
        }]);
        tl.next_link = 1;
        let before = tl.snapshot();

        tl.split_at(4.0);
        let first = tl.tracks[0].clips[1].link;

        tl.restore(&before);
        tl.split_at(4.0);
        // The rewind is what keeps ids from drifting upward on every
        // undo/redo cycle; the clip that held the old id is gone.
        assert_eq!(tl.tracks[0].clips[1].link, first);
    }

    #[test]
    fn a_split_keeps_the_left_half_s_name_and_coins_one_for_the_right() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        let original = tl.tracks[0].clips[0].id;

        tl.split_at(4.0);
        assert_eq!(tl.tracks[0].clips[0].id, original);
        assert_ne!(tl.tracks[0].clips[1].id, original);
    }

    #[test]
    fn every_clip_a_split_produces_has_its_own_name() {
        // Two tracks split at once: the right halves must not collide, or a
        // selection would resolve to whichever the search reached first.
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        tl.tracks.push(Track::new(TrackKind::Audio));
        tl.tracks[1].clips = vec![Clip {
            id: tl.new_clip_id(),
            ..clip(0.0, 10.0)
        }];

        tl.split_at(4.0);
        let ids: Vec<u32> = tl
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.id))
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "duplicate clip ids in {ids:?}");
    }

    #[test]
    fn find_locates_a_clip_and_forgets_a_deleted_one() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0), clip(10.0, 5.0)]);
        let second = tl.tracks[0].clips[1].id;
        assert_eq!(tl.find(second), Some((0, 1)));

        // Removing the clip in front of it shifts its index, which is the whole
        // reason selection is held by id rather than position.
        tl.tracks[0].clips.remove(0);
        assert_eq!(tl.find(second), Some((0, 0)));

        tl.tracks[0].clips.clear();
        assert_eq!(tl.find(second), None);
    }

    #[test]
    fn restore_rewinds_clip_ids_so_a_redone_split_reuses_them() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        let before = tl.snapshot();

        tl.split_at(4.0);
        let first = tl.tracks[0].clips[1].id;

        tl.restore(&before);
        tl.split_at(4.0);
        // Without the rewind, ids would climb on every undo/redo cycle and a
        // selection restored by undo would no longer match its clip.
        assert_eq!(tl.tracks[0].clips[1].id, first);
    }

    #[test]
    fn snapshot_is_a_deep_copy() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        let before = tl.snapshot();
        tl.tracks[0].clips[0].timeline_start = 99.0;
        assert_ne!(tl.snapshot(), before);

        tl.restore(&before);
        assert_eq!(tl.tracks[0].clips[0].timeline_start, 0.0);
    }

    #[test]
    fn edit_points_from_mid_clip_are_that_clip_s_own_bounds() {
        let tl = timeline_with(vec![clip(0.0, 10.0), clip(10.0, 5.0)]);
        assert_eq!(tl.prev_edit_point(4.0), Some(0.0));
        assert_eq!(tl.next_edit_point(4.0), Some(10.0));
    }

    #[test]
    fn repeated_jumps_walk_past_the_current_boundary() {
        let tl = timeline_with(vec![clip(0.0, 10.0), clip(10.0, 5.0)]);
        // Standing exactly on the A|B seam: forward must reach B's end and
        // back must reach A's start, rather than dead-ending where we are.
        assert_eq!(tl.next_edit_point(10.0), Some(15.0));
        assert_eq!(tl.prev_edit_point(10.0), Some(0.0));
    }

    #[test]
    fn edit_points_terminate_at_the_ends() {
        let tl = timeline_with(vec![clip(0.0, 10.0)]);
        assert_eq!(tl.prev_edit_point(0.0), None);
        assert_eq!(tl.next_edit_point(10.0), None);
    }

    #[test]
    fn zero_is_reachable_when_the_first_clip_starts_later() {
        let tl = timeline_with(vec![clip(5.0, 10.0)]);
        assert_eq!(tl.prev_edit_point(3.0), Some(0.0));
    }

    #[test]
    fn edit_points_span_every_track() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        tl.tracks.push(Track::new(TrackKind::Audio));
        tl.tracks[1].clips = vec![clip(3.0, 2.0)];
        // The audio-only boundary at 3.0 is navigable from the video clip.
        assert_eq!(tl.next_edit_point(1.0), Some(3.0));
        assert_eq!(tl.next_edit_point(3.0), Some(5.0));
    }

    #[test]
    fn restore_reinstates_clips_removed_with_their_source() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0), clip(20.0, 5.0)]);
        let before = tl.snapshot();

        tl.remove_source(SourceId(0));
        assert!(tl.tracks[0].clips.is_empty());

        tl.restore(&before);
        assert_eq!(tl.tracks[0].clips.len(), 2);
        assert_eq!(tl.duration(), 25.0);
    }
}
