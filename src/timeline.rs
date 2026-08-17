use serde::{Deserialize, Serialize};

/// Slack when testing "is the playhead already on this boundary". Jumping to a
/// boundary lands the playhead on it exactly, but the position round-trips
/// through the audio engine, so an exact compare would let float drift strand
/// the next jump.
const EDIT_POINT_EPS: f64 = 1e-6;

/// The level line's range, in decibels. The bottom is a floor rather than true
/// silence: it keeps the dB mapping finite, and a clip you want actually silent
/// is one you fade out or delete. The top is the usual +6 of headroom — enough
/// to lift a quiet take, not enough to invite clipping the mix.
pub const MIN_GAIN_DB: f32 = -40.0;
pub const MAX_GAIN_DB: f32 = 6.0;

pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Inverse of [`db_to_gain`], floored at [`MIN_GAIN_DB`] so a zero gain maps to
/// the bottom of the line rather than to negative infinity.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        return MIN_GAIN_DB;
    }
    (20.0 * gain.log10()).max(MIN_GAIN_DB)
}

/// Which end of a clip a fade hangs off.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FadeSide {
    In,
    Out,
}

/// Serde's default for [`Clip::gain`]. A file written before clips had a level
/// has to load at unity, not at silence.
fn unity_gain() -> f32 {
    1.0
}

/// `transparent` so a project file writes `source: 0` rather than
/// `source: SourceId(0)` — the wrapper earns its keep in the type system, not
/// on disk.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Linear level multiplier for this clip's audio; 1.0 is unity.
    ///
    /// Linear rather than decibels because that is what the mixer multiplies
    /// by, and a value that has to be converted on every sample is the wrong
    /// one to store. The UI converts the other way, once per drag.
    #[serde(default = "unity_gain")]
    pub gain: f32,
    /// Fade lengths in seconds, measured inward from each end of the clip.
    ///
    /// Durations rather than absolute times, so trimming the head of a clip
    /// leaves the fade on the head rather than stranding it mid-clip.
    #[serde(default)]
    pub fade_in: f64,
    #[serde(default)]
    pub fade_out: f64,
}

/// The neutral clip: unity level, no fades. Construction sites spread `..` over
/// this rather than restating three zeroes each, so a clip that gains another
/// parameter later doesn't have to touch every site that makes one.
impl Default for Clip {
    fn default() -> Self {
        Self {
            id: 0,
            source: SourceId(0),
            source_in: 0.0,
            source_out: 0.0,
            timeline_start: 0.0,
            link: None,
            gain: 1.0,
            fade_in: 0.0,
            fade_out: 0.0,
        }
    }
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

    /// Level multiplier at timeline time `t` — the clip's gain, tapered by
    /// whichever fade `t` falls inside.
    ///
    /// The taper is linear in amplitude, which is what an editor's fade handle
    /// has always meant; a curve would be a second decision on top of the
    /// length and belongs to a fade that can be told which shape it is.
    ///
    /// Both fades multiply, so a clip short enough to hold overlapping ones
    /// still reaches zero at each end instead of jumping. Progress is clamped
    /// rather than windowed: the mixer resolves which clip is live once per
    /// chunk and can run a few milliseconds past a boundary, and holding the
    /// end level there beats letting the ramp carry on through zero.
    pub fn level(&self, t: f64) -> f32 {
        if self.fade_in <= 0.0 && self.fade_out <= 0.0 {
            return self.gain;
        }
        let mut f = 1.0_f64;
        if self.fade_in > 0.0 {
            f *= ((t - self.timeline_start) / self.fade_in).clamp(0.0, 1.0);
        }
        if self.fade_out > 0.0 {
            f *= ((self.timeline_end() - t) / self.fade_out).clamp(0.0, 1.0);
        }
        self.gain * f as f32
    }

    /// Pull the fades back inside the clip, and off each other.
    ///
    /// Called after anything that changes a clip's length. A trim that
    /// shortens a clip past its own fade would otherwise leave a ramp that
    /// never finishes — audible as a clip that plays entirely under its
    /// intended level. The head is clamped first and the tail takes what is
    /// left, so shrinking a clip eats the fade-out before the fade-in.
    pub fn clamp_fades(&mut self) {
        let dur = self.duration();
        self.fade_in = self.fade_in.clamp(0.0, dur);
        self.fade_out = self.fade_out.clamp(0.0, dur - self.fade_in);
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

    /// Re-derive the id counters from the clips actually present, so the next
    /// id handed out is clear of every one in use.
    ///
    /// This is how a loaded project gets its counters, rather than storing
    /// them in the file. Uniqueness is the only thing these ids owe anyone,
    /// and deriving them means a file that was hand-edited — or trimmed by an
    /// older build — can't seed a counter low enough to reissue an id some
    /// clip already holds.
    pub fn reseed_counters(&mut self) {
        let clips = || self.tracks.iter().flat_map(|t| t.clips.iter());
        self.next_clip = clips().map(|c| c.id + 1).max().unwrap_or(0);
        self.next_link = clips().filter_map(|c| c.link.map(|l| l + 1)).max().unwrap_or(0);
    }

    /// The first audio track with nothing occupying `[start, end)`, or `None`
    /// when every one of them is busy there.
    ///
    /// Auto-pairing a video drop places the audio half for you, and placing it
    /// on A1 regardless would bury it under whatever was already there: two
    /// clips over the same instant on one track, of which the mixer plays
    /// whichever it happens to find first.
    pub fn free_audio_track(&self, start: f64, end: f64) -> Option<usize> {
        self.tracks.iter().position(|track| {
            track.kind == TrackKind::Audio
                && !track
                    .clips
                    .iter()
                    .any(|c| c.timeline_start < end && c.timeline_end() > start)
        })
    }

    /// Append an empty track of `kind`, and return its index.
    ///
    /// Position in the vector is only an ordering within the kind — see
    /// [`crate::State::visual_index`] — so a track added here becomes the
    /// bottom lane of its half of the timeline.
    pub fn push_track(&mut self, kind: TrackKind) -> usize {
        self.tracks.push(Track::new(kind));
        self.tracks.len() - 1
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
                    // Each fade stays with the end it hangs off: the cut is a
                    // hard one, and inventing a ramp at it would be a decision
                    // the split never made.
                    track.clips[i].fade_out = 0.0;
                    track.clips[i].clamp_fades();
                    let right_link = orig.link.map(|old| relink[&old]);
                    // The left half keeps the original id, so a selection on
                    // the clip you split stays on the part before the cut.
                    let right_id = next_clip;
                    next_clip += 1;
                    let mut right = Clip {
                        id: right_id,
                        source_in: split_source_t,
                        timeline_start: t,
                        link: right_link,
                        fade_in: 0.0,
                        ..orig
                    };
                    right.clamp_fades();
                    track.clips.insert(i + 1, right);
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
            source_out: dur,
            timeline_start: start,
            ..Clip::default()
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
    fn reseeding_clears_every_id_in_use() {
        let mut tl = timeline_with(vec![
            Clip { id: 7, link: Some(3), ..clip(0.0, 10.0) },
            Clip { id: 2, link: None, ..clip(10.0, 5.0) },
        ]);
        // `timeline_with` renumbers, so put the ids we care about back.
        tl.tracks[0].clips[0].id = 7;
        tl.tracks[0].clips[1].id = 2;
        tl.next_clip = 0;
        tl.next_link = 0;

        tl.reseed_counters();
        assert_eq!(tl.new_clip_id(), 8);
        assert_eq!(tl.new_link_id(), 4);
    }

    /// A project saved with nothing on the timeline must not start its
    /// counters somewhere odd.
    #[test]
    fn reseeding_an_empty_timeline_starts_from_zero() {
        let mut tl = timeline_with(vec![]);
        tl.next_clip = 99;
        tl.reseed_counters();
        assert_eq!(tl.new_clip_id(), 0);
        assert_eq!(tl.new_link_id(), 0);
    }

    /// Unlinked clips must not drag the link counter up to their clip ids.
    #[test]
    fn reseeding_ignores_clip_ids_when_seeding_links() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0); 5]);
        tl.reseed_counters();
        assert_eq!(tl.new_link_id(), 0);
    }

    fn assert_level(clip: &Clip, t: f64, want: f32) {
        let got = clip.level(t);
        assert!((got - want).abs() < 1e-6, "level at {t}: got {got}, want {want}");
    }

    #[test]
    fn a_clip_with_no_fades_plays_at_its_gain() {
        let c = Clip { gain: 0.5, ..clip(0.0, 10.0) };
        assert_level(&c, 0.0, 0.5);
        assert_level(&c, 5.0, 0.5);
    }

    #[test]
    fn a_fade_ramps_from_silence_to_the_clip_s_gain() {
        let c = Clip { gain: 0.5, fade_in: 2.0, ..clip(0.0, 10.0) };
        assert_level(&c, 0.0, 0.0);
        assert_level(&c, 1.0, 0.25);
        assert_level(&c, 2.0, 0.5);
        // Past the ramp the gain holds rather than continuing to climb.
        assert_level(&c, 6.0, 0.5);
    }

    #[test]
    fn a_fade_out_is_measured_back_from_the_clip_s_end() {
        // Starting at 5 rather than 0: the fade hangs off the end, so moving
        // the clip must not move where the ramp begins relative to it.
        let c = Clip { fade_out: 4.0, ..clip(5.0, 10.0) };
        assert_level(&c, 10.0, 1.0);
        assert_level(&c, 13.0, 0.5);
        assert_level(&c, 15.0, 0.0);
    }

    /// The mixer resolves which clip is live once per chunk and can ask for a
    /// level a few milliseconds past the boundary. That must hold the end
    /// value, not carry the ramp on through zero into negative gain.
    #[test]
    fn a_level_asked_for_past_the_clip_holds_rather_than_inverting() {
        let c = Clip { fade_out: 2.0, ..clip(0.0, 10.0) };
        assert_level(&c, 10.5, 0.0);
        assert_level(&c, -0.5, 1.0);
    }

    #[test]
    fn overlapping_fades_still_reach_silence_at_both_ends() {
        let mut c = Clip { fade_in: 8.0, fade_out: 8.0, ..clip(0.0, 10.0) };
        // Clamping is what keeps them from overlapping in the first place; the
        // head keeps its length and the tail takes what is left.
        c.clamp_fades();
        assert_eq!(c.fade_in, 8.0);
        assert_eq!(c.fade_out, 2.0);
        assert_level(&c, 0.0, 0.0);
        assert_level(&c, 10.0, 0.0);
    }

    #[test]
    fn trimming_a_clip_shorter_pulls_its_fades_in() {
        let mut c = Clip { fade_in: 3.0, fade_out: 3.0, ..clip(0.0, 10.0) };
        c.source_out = 4.0;
        c.clamp_fades();
        assert_eq!(c.fade_in, 3.0);
        assert_eq!(c.fade_out, 1.0);
        // A ramp that outlived its clip would leave the whole thing playing
        // under its intended level; this one still reaches full gain.
        assert_level(&c, 3.0, 1.0);
    }

    #[test]
    fn a_split_leaves_each_fade_on_the_end_it_hangs_off() {
        let mut tl = timeline_with(vec![Clip {
            gain: 0.5,
            fade_in: 1.0,
            fade_out: 1.0,
            ..clip(0.0, 10.0)
        }]);
        tl.split_at(4.0);
        let (left, right) = (tl.tracks[0].clips[0], tl.tracks[0].clips[1]);
        assert_eq!((left.fade_in, left.fade_out), (1.0, 0.0));
        assert_eq!((right.fade_in, right.fade_out), (0.0, 1.0));
        // The cut is hard, and both halves keep the level they were set to.
        assert_eq!((left.gain, right.gain), (0.5, 0.5));
    }

    /// A fade longer than the half it lands in has to come back inside it, or
    /// the shorter half plays entirely under level.
    #[test]
    fn a_split_pulls_a_fade_inside_the_half_that_keeps_it() {
        let mut tl = timeline_with(vec![Clip { fade_out: 6.0, ..clip(0.0, 10.0) }]);
        tl.split_at(8.0);
        assert_eq!(tl.tracks[0].clips[1].fade_out, 2.0);
    }

    #[test]
    fn decibels_round_trip_through_linear_gain() {
        for db in [-40.0, -12.0, -6.0, 0.0, 6.0] {
            let back = gain_to_db(db_to_gain(db));
            assert!((back - db).abs() < 1e-4, "{db} came back as {back}");
        }
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
        // Silence has no decibel value; the floor stands in for it so the
        // level line has somewhere to put a muted clip.
        assert_eq!(gain_to_db(0.0), MIN_GAIN_DB);
    }

    #[test]
    fn a_paired_drop_looks_past_an_audio_track_that_is_busy() {
        let mut tl = timeline_with(vec![]);
        tl.tracks.push(Track::new(TrackKind::Audio));
        tl.tracks.push(Track::new(TrackKind::Audio));
        tl.tracks[1].clips = vec![clip(0.0, 10.0)];

        // A1 is busy where the drop lands, so the audio goes to A2.
        assert_eq!(tl.free_audio_track(4.0, 6.0), Some(2));
        // Clear of it, A1 is free again — abutting doesn't count as overlap.
        assert_eq!(tl.free_audio_track(10.0, 20.0), Some(1));
    }

    #[test]
    fn every_audio_track_being_busy_is_answered_with_none() {
        let mut tl = timeline_with(vec![]);
        tl.tracks.push(Track::new(TrackKind::Audio));
        tl.tracks[1].clips = vec![clip(0.0, 10.0)];
        assert_eq!(tl.free_audio_track(1.0, 2.0), None);

        // Which is what a drop turns into a new lane at the bottom of the
        // audio half.
        assert_eq!(tl.push_track(TrackKind::Audio), 2);
        assert_eq!(tl.free_audio_track(1.0, 2.0), Some(2));
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
