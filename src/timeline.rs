use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::effects::EffectStack;

/// Slack when testing "is the playhead already on this boundary". Jumping to a
/// boundary lands the playhead on it exactly, but the position round-trips
/// through the audio engine, so an exact compare would let float drift strand
/// the next jump.
const EDIT_POINT_EPS: f64 = 1e-6;

/// A video clip's opacity range. The top is fully opaque; there is nothing
/// above it, unlike audio's +12dB of headroom — a picture cannot be more than
/// itself.
pub const MIN_OPACITY: f32 = 0.0;
pub const MAX_OPACITY: f32 = 1.0;

/// How far a clip's picture may be scaled off its default fit. The floor keeps
/// a clip that has been shrunk to nothing still grabbable. The ceiling leaves
/// room to zoom well into footage cropped to another shape — landscape into a
/// vertical canvas fills it at about 316% before any zoom of its own — and
/// costs nothing in the renderers: the export cuts out the part on canvas
/// before it scales, and blurs at the source's own size, so neither works on
/// a picture bigger than the source or the canvas however far in this goes.
/// Kept a constant here rather than a number the drag happens to stop at, and
/// 0.05 to 20 puts 100% in the middle of the zoom slider's log scale.
pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 20.0;

/// The level line's range, in decibels. The bottom is a floor rather than true
/// silence: it keeps the dB mapping finite, and a clip you want actually silent
/// is one you fade out or delete. The top is +12 of headroom, Final Cut's ceiling:
/// enough to rescue a quiet take, and the waveform turns red where a boost
/// takes it past full scale.
pub const MIN_GAIN_DB: f32 = -40.0;
pub const MAX_GAIN_DB: f32 = 12.0;

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

/// Where a clip's picture sits on the canvas, relative to the aspect-preserving
/// fit that used to be the only option.
///
/// Deliberately not in canvas pixels. An offset in pixels would mean a project
/// re-mastered from 1080p to 4K moved every overlay to a quarter of the way in;
/// as a fraction of the canvas it lands in the same place on any canvas. Scale
/// is a multiplier on the fit for the same reason: `1.0` is "as large as it
/// goes without cropping", whatever the two formats are.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    /// Offset from centred, in canvas widths and canvas heights.
    pub x: f32,
    pub y: f32,
    pub scale: f32,
}

/// Centred and filling the canvas: the neutral placement a clip gets until it
/// is moved or scaled.
impl Default for Transform {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, scale: 1.0 }
    }
}

/// One of a placement's numbers, as the effects tab's sliders set them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransformField {
    Zoom,
    X,
    Y,
}

impl TransformField {
    pub const ALL: [TransformField; 3] = [Self::Zoom, Self::X, Self::Y];

    pub fn name(self) -> &'static str {
        match self {
            Self::Zoom => "Zoom",
            Self::X => "Position X",
            Self::Y => "Position Y",
        }
    }

    pub fn get(self, t: &Transform) -> f32 {
        match self {
            Self::Zoom => t.scale,
            Self::X => t.x,
            Self::Y => t.y,
        }
    }

    pub fn set(self, t: &mut Transform, value: f32) {
        match self {
            Self::Zoom => t.scale = value,
            Self::X => t.x = value,
            Self::Y => t.y = value,
        }
    }
}

/// `transparent` so a project file writes `source: 0` rather than
/// `source: SourceId(0)` — the wrapper earns its keep in the type system, not
/// on disk.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    /// What the track on lane `lane` of this kind is called: V1, A2.
    pub fn label(self, lane: usize) -> String {
        let letter = match self {
            TrackKind::Video => 'V',
            TrackKind::Audio => 'A',
        };
        format!("{letter}{}", lane + 1)
    }
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
    /// The link switched off: the clip is moved, trimmed, split and deleted
    /// on its own, while `link` goes on naming the clips it was paired with
    /// so the pair can be put back together. Read through
    /// [`Clip::live_link`]. Defaulted so a project saved before a link could
    /// be switched off still opens.
    #[serde(default)]
    pub unlinked: bool,
    /// Linear level multiplier for this clip's audio; 1.0 is unity.
    ///
    /// Linear rather than decibels because that is what the mixer multiplies
    /// by, and a value that has to be converted on every sample is the wrong
    /// one to store. The UI converts the other way, once per drag.
    pub gain: f32,
    /// Fade lengths in seconds, measured inward from each end of the clip.
    ///
    /// Durations rather than absolute times, so trimming the head of a clip
    /// leaves the fade on the head rather than stranding it mid-clip.
    pub fade_in: f64,
    pub fade_out: f64,
    /// How much of this clip's picture reaches the canvas; 1.0 is opaque.
    ///
    /// The picture's counterpart to [`Clip::gain`], and deliberately a separate
    /// field rather than the same one read two ways: a link says two clips
    /// share a position in time, not a level, and the two quantities have
    /// different ranges — audio has headroom above unity and a picture does
    /// not. A clip only ever lives on one kind of track, so only one of the
    /// pair is ever live; the other costs four bytes and keeps the meaning
    /// unambiguous.
    pub opacity: f32,
    /// Where the picture lands on the canvas. See [`Transform`].
    pub transform: Transform,
    /// What is done to the picture before it lands, in order. Defaulted so a
    /// project saved before clips had effects still opens.
    #[serde(default)]
    pub effects: EffectStack,
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
            unlinked: false,
            gain: 1.0,
            fade_in: 0.0,
            fade_out: 0.0,
            opacity: 1.0,
            transform: Transform::default(),
            effects: EffectStack::default(),
        }
    }
}

impl Clip {
    /// The link this clip follows: its link id, unless that is switched off.
    pub fn live_link(&self) -> Option<u32> {
        self.link.filter(|_| !self.unlinked)
    }

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

    /// How much of the clip's fades have opened at timeline time `t`, from 0
    /// (shut) to 1 (fully open).
    ///
    /// The taper is linear, which is what an editor's fade handle has always
    /// meant; a curve would be a second decision on top of the length and
    /// belongs to a fade that can be told which shape it is.
    ///
    /// Both fades multiply, so a clip short enough to hold overlapping ones
    /// still reaches zero at each end instead of jumping. Progress is clamped
    /// rather than windowed: the mixer resolves which clip is live once per
    /// chunk and can run a few milliseconds past a boundary, and holding the
    /// end level there beats letting the ramp carry on through zero.
    ///
    /// One ramp shared by the sound and the picture, so the two halves of a
    /// linked pair fade over exactly the same span — and so a dissolve between
    /// two overlapping video clips is the same gesture as an audio crossfade
    /// rather than a second concept that happens to look like one.
    pub fn fade_factor(&self, t: f64) -> f64 {
        if self.fade_in <= 0.0 && self.fade_out <= 0.0 {
            return 1.0;
        }
        let mut f = 1.0_f64;
        if self.fade_in > 0.0 {
            f *= ((t - self.timeline_start) / self.fade_in).clamp(0.0, 1.0);
        }
        if self.fade_out > 0.0 {
            f *= ((self.timeline_end() - t) / self.fade_out).clamp(0.0, 1.0);
        }
        f
    }

    /// Level multiplier at timeline time `t` — the clip's gain, tapered by
    /// whichever fade `t` falls inside.
    pub fn level(&self, t: f64) -> f32 {
        self.gain * self.fade_factor(t) as f32
    }

    /// The picture's counterpart to [`Clip::level`]: how much of this clip
    /// reaches the canvas at `t`, its opacity tapered by the same fades.
    pub fn alpha(&self, t: f64) -> f32 {
        (self.opacity * self.fade_factor(t) as f32).clamp(0.0, 1.0)
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
}

/// What a compound clip holds: tracks of its own, timed from zero, in the
/// same order and with the same meaning as the timeline's.
pub type Tracks = Vec<(TrackKind, Vec<Clip>)>;

/// How deep compound clips may sit inside each other before the rest is
/// ignored. Far past any real edit; the bound is what keeps a cycle — which
/// the editor refuses to make, but a hand-edited file could hold — from
/// recursing forever.
pub const MAX_NESTING: usize = 8;

/// How long a compound clip's contents run: to the end of its last clip.
pub fn tracks_duration(tracks: &[(TrackKind, Vec<Clip>)]) -> f64 {
    tracks
        .iter()
        .flat_map(|(_, clips)| clips.iter().map(Clip::timeline_end))
        .fold(0.0, f64::max)
}

/// `inner`, a clip inside a compound, as it plays on the timeline the
/// compound clip `outer` sits on: moved to where `outer` puts it, and cut to
/// the part of it `outer` shows. `None` when `outer` is trimmed past all of it.
pub fn map_into(outer: &Clip, inner: &Clip) -> Option<Clip> {
    let offset = outer.timeline_start - outer.source_in;
    let s = inner.timeline_start + offset;
    let e = s + inner.duration();
    let lo = s.max(outer.timeline_start);
    let hi = e.min(outer.timeline_end());
    if hi <= lo {
        return None;
    }
    let mut c = *inner;
    c.timeline_start = lo;
    c.source_in = inner.source_in + (lo - s);
    c.source_out = c.source_in + (hi - lo);
    c.clamp_fades();
    Some(c)
}

/// Every audio clip the mixers play, as tracks: the timeline's own audio
/// tracks, then the audio inside each compound clip, moved onto the timeline
/// by [`map_into`]. A compound clip's sound is part of it, the way footage's
/// is, so it plays wherever the compound clip does.
///
/// Compound clips on one track never overlap, so the audio of each inner
/// track is gathered onto one track per outer one, however many compound
/// clips that track holds.
pub fn flatten_audio<'a>(
    tracks: &[(TrackKind, &'a [Clip])],
    contents_of: &dyn Fn(SourceId) -> Option<&'a [(TrackKind, Vec<Clip>)]>,
) -> Vec<Vec<Clip>> {
    flatten_audio_at(tracks, contents_of, 0)
}

fn flatten_audio_at<'a>(
    tracks: &[(TrackKind, &'a [Clip])],
    contents_of: &dyn Fn(SourceId) -> Option<&'a [(TrackKind, Vec<Clip>)]>,
    depth: usize,
) -> Vec<Vec<Clip>> {
    let mut out = Vec::new();
    for &(kind, clips) in tracks {
        if kind == TrackKind::Audio {
            out.push(clips.to_vec());
            continue;
        }
        if depth >= MAX_NESTING {
            continue;
        }
        let mut lanes: Vec<Vec<Clip>> = Vec::new();
        for outer in clips {
            let Some(inner) = contents_of(outer.source) else {
                continue;
            };
            let inner: Vec<(TrackKind, &[Clip])> =
                inner.iter().map(|(k, c)| (*k, c.as_slice())).collect();
            for (i, lane) in flatten_audio_at(&inner, contents_of, depth + 1).iter().enumerate() {
                if lanes.len() <= i {
                    lanes.push(Vec::new());
                }
                lanes[i].extend(lane.iter().filter_map(|c| map_into(outer, c)));
            }
        }
        out.extend(lanes.into_iter().filter(|l| !l.is_empty()));
    }
    out
}

/// Clips copied out of the timeline, to be pasted back in elsewhere: each with
/// the lane it came from, so a paste puts every clip back on its own.
///
/// A lane is a track's place among the tracks of its kind — the number in its
/// V2 label, counted from zero — rather than its index, which only means
/// anything in the timeline the copy was made in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Clipboard {
    pub clips: Vec<(usize, TrackKind, Clip)>,
}

impl Clipboard {
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// The lowest lane of `kind` any of the clips came off, if any did.
    pub fn lowest(&self, kind: TrackKind) -> Option<usize> {
        self.clips.iter().filter(|(_, k, _)| *k == kind).map(|(lane, _, _)| *lane).min()
    }
}

/// A lane of each kind, where one has been picked: the tracks a paste is
/// aimed at. See [`Timeline::paste`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PasteLanes {
    pub video: Option<usize>,
    pub audio: Option<usize>,
}

impl PasteLanes {
    pub fn of(&self, kind: TrackKind) -> Option<usize> {
        match kind {
            TrackKind::Video => self.video,
            TrackKind::Audio => self.audio,
        }
    }

    pub fn of_mut(&mut self, kind: TrackKind) -> &mut Option<usize> {
        match kind {
            TrackKind::Video => &mut self.video,
            TrackKind::Audio => &mut self.audio,
        }
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

    /// The tracks as they are, in the form a compound clip holds them.
    pub fn to_tracks(&self) -> Tracks {
        self.tracks.iter().map(|t| (t.kind, t.clips.clone())).collect()
    }

    /// A timeline holding `tracks`, for editing a compound clip's contents.
    pub fn from_tracks(tracks: &[(TrackKind, Vec<Clip>)]) -> Self {
        let mut tl = Self::new();
        tl.tracks = tracks
            .iter()
            .map(|(kind, clips)| Track { kind: *kind, clips: clips.clone() })
            .collect();
        tl.reseed_counters();
        tl
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

    /// Whether another clip carries `clip`'s link id, switched on or off: the
    /// other half its chain badge stands for. A clip whose partner has been
    /// deleted has nothing left to unlink from, or to link back to.
    pub fn has_partner(&self, clip: &Clip) -> bool {
        clip.link.is_some()
            && self
                .tracks
                .iter()
                .flat_map(|t| &t.clips)
                .any(|c| c.id != clip.id && c.link == clip.link)
    }

    /// Switch link `link` off (`unlinked`) or back on, on every clip that
    /// carries it.
    pub fn set_unlinked(&mut self, link: u32, unlinked: bool) {
        for clip in self.tracks.iter_mut().flat_map(|t| &mut t.clips) {
            if clip.link == Some(link) {
                clip.unlinked = unlinked;
            }
        }
    }

    /// The track on lane `lane` of `kind`: lane 0 of video is the one labelled
    /// V1.
    pub fn track_on_lane(&self, kind: TrackKind, lane: usize) -> Option<usize> {
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == kind)
            .map(|(i, _)| i)
            .nth(lane)
    }

    /// Move track `idx` one place up the timeline as drawn (`up`), or down,
    /// past the next track of its kind: for video, up the stack, so it draws
    /// over the one it passes. Returns where it went, or `None` when it is
    /// already at that end.
    ///
    /// Video lanes are drawn upward from the middle and audio lanes downward,
    /// so up is a later track of its kind for video and an earlier one for
    /// audio.
    pub fn move_track(&mut self, idx: usize, up: bool) -> Option<usize> {
        let kind = self.tracks.get(idx)?.kind;
        let later = match kind {
            TrackKind::Video => up,
            TrackKind::Audio => !up,
        };
        let same = |i: &usize| self.tracks[*i].kind == kind;
        let to = if later {
            (idx + 1..self.tracks.len()).find(same)?
        } else {
            (0..idx).rev().find(same)?
        };
        self.tracks.swap(idx, to);
        Some(to)
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

    /// Split every clip containing `t` that `which` picks into two clips meeting
    /// at `t`. Clips whose start aligns exactly with `t` are left alone —
    /// there's nothing to split.
    ///
    /// Link preservation: the left halves keep the original link id; the right
    /// halves get a new shared link id (per old link id) so that e.g. video +
    /// audio clips linked together end up with their right halves linked to
    /// each other. Unlinked clips stay unlinked.
    pub fn split_at(&mut self, t: f64, which: impl Fn(&Clip) -> bool) {
        use std::collections::{HashMap, HashSet};

        // First pass: find every old link id on a clip that will actually split.
        // We do this up front so we can allocate new ids without holding a
        // mutable borrow on `self.tracks` while also bumping `self.next_link`.
        let mut old_links: HashSet<u32> = HashSet::new();
        for track in &self.tracks {
            for clip in &track.clips {
                if which(clip) && clip.contains(t) && t > clip.timeline_start {
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
                if which(&orig) && orig.contains(t) && t > orig.timeline_start {
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

impl Timeline {
    /// The clips a ripple from `t` carries: every one starting at or after
    /// it, on every track, but those in `skip` — the clip being trimmed and
    /// what is linked to it, whose ends move by trimming instead.
    pub fn followers(&self, t: f64, skip: &HashSet<u32>) -> HashSet<u32> {
        self.tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .filter(|c| c.timeline_start >= t - EDIT_POINT_EPS && !skip.contains(&c.id))
            .map(|c| c.id)
            .collect()
    }

    /// How far `followers` can move earlier together: until one of them runs
    /// into a clip that stays where it is on its own track, or into the start
    /// of the timeline. Clips in `trimmed` stay at their start but take their
    /// end along with the followers, so they never close in on one.
    pub fn ripple_room(&self, followers: &HashSet<u32>, trimmed: &HashSet<u32>) -> f64 {
        let mut room = f64::INFINITY;
        for track in &self.tracks {
            for f in track.clips.iter().filter(|c| followers.contains(&c.id)) {
                room = room.min(f.timeline_start);
                for s in &track.clips {
                    if followers.contains(&s.id) || trimmed.contains(&s.id) {
                        continue;
                    }
                    if s.timeline_end() <= f.timeline_start + EDIT_POINT_EPS {
                        room = room.min(f.timeline_start - s.timeline_end());
                    }
                }
            }
        }
        room.max(0.0)
    }

    /// Move the clips called `ids` by `delta` seconds.
    pub fn shift_clips(&mut self, ids: &HashSet<u32>, delta: f64) {
        for track in &mut self.tracks {
            for clip in track.clips.iter_mut().filter(|c| ids.contains(&c.id)) {
                clip.timeline_start += delta;
            }
        }
    }

    /// Make room for `len` seconds at `t`: cut the clips `which` picks where
    /// they cross `t`, then move every clip starting at or after `t` that much
    /// later, on every track, so the rest of the edit keeps its timing against
    /// itself. A clip that `which` leaves alone and that crosses `t` stays as
    /// it is — a music bed carries on under whatever fills the gap.
    pub fn open_gap(&mut self, t: f64, len: f64, which: impl Fn(&Clip) -> bool) {
        self.split_at(t, which);
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.timeline_start >= t - EDIT_POINT_EPS {
                    clip.timeline_start += len;
                }
            }
        }
    }
}

impl TimelineSnapshot {
    pub fn tracks(&self) -> &[(TrackKind, Vec<Clip>)] {
        &self.tracks
    }
}

impl Timeline {
    /// The clips called `ids`, for pasting later.
    pub fn copy(&self, ids: &std::collections::HashSet<u32>) -> Clipboard {
        let mut clips = Vec::new();
        let (mut video, mut audio) = (0, 0);
        for track in &self.tracks {
            let next = match track.kind {
                TrackKind::Video => &mut video,
                TrackKind::Audio => &mut audio,
            };
            let lane = *next;
            *next += 1;
            clips.extend(
                track.clips.iter().filter(|c| ids.contains(&c.id)).map(|c| (lane, track.kind, *c)),
            );
        }
        Clipboard { clips }
    }

    /// Put copies of `board`'s clips in, keeping their spacing and their
    /// lanes, with the earliest of them at `at` — or, where that would land
    /// on a clip already there, as soon after it as the whole group fits.
    /// Returns the new clips' ids.
    ///
    /// A lane in `onto` moves the clips of its kind there: the lowest lane
    /// they were copied from lands on it, and the others stay as far above
    /// it as they were. The kind without one goes back on the lanes it came
    /// from.
    ///
    /// The copies are clips of their own: fresh ids, and links of their own
    /// that pair them with each other rather than with the clips they came
    /// from. A lane that is not there — its track gone since the copy, or
    /// this timeline never that tall — gets a new track.
    pub fn paste(&mut self, board: &Clipboard, at: f64, onto: PasteLanes) -> Vec<u32> {
        let Some(start) = board.clips.iter().map(|(_, _, c)| c.timeline_start).reduce(f64::min)
        else {
            return Vec::new();
        };
        // The track each copied lane goes onto. Lanes come in rising order
        // within a kind, as `copy` leaves them, so tracks made here stack in
        // the order the clips did.
        let mut lanes = std::collections::HashMap::new();
        for &(lane, kind, _) in &board.clips {
            if lanes.contains_key(&(lane, kind)) {
                continue;
            }
            let wanted = match (onto.of(kind), board.lowest(kind)) {
                (Some(target), Some(low)) => target + (lane - low),
                _ => lane,
            };
            let track = match self.track_on_lane(kind, wanted) {
                Some(track) => track,
                None => self.push_track(kind),
            };
            lanes.insert((lane, kind), track);
        }

        // Pushed later until nothing is in the way. Each push clears the clip
        // it found and only ever moves right, so this ends: at worst past the
        // last clip on every track.
        const EPS: f64 = 1e-9;
        let mut shift = at.max(0.0) - start;
        'fit: loop {
            for (lane, kind, clip) in &board.clips {
                let s = clip.timeline_start + shift;
                let e = s + clip.duration();
                let blocker = self.tracks[lanes[&(*lane, *kind)]]
                    .clips
                    .iter()
                    .filter(|o| o.timeline_start < e - EPS && o.timeline_end() > s + EPS)
                    .map(Clip::timeline_end)
                    .reduce(f64::max);
                if let Some(end) = blocker {
                    shift += end - s;
                    continue 'fit;
                }
            }
            break;
        }

        let mut links = std::collections::HashMap::new();
        let mut ids = Vec::with_capacity(board.clips.len());
        for (lane, kind, clip) in &board.clips {
            let link = clip.link.map(|old| *links.entry(old).or_insert_with(|| {
                let id = self.next_link;
                self.next_link += 1;
                id
            }));
            let id = self.new_clip_id();
            self.tracks[lanes[&(*lane, *kind)]].clips.push(Clip {
                id,
                link,
                timeline_start: clip.timeline_start + shift,
                ..*clip
            });
            ids.push(id);
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clip inside a compound clip plays where the compound clip puts it,
    /// cut to the part of the contents the compound clip is trimmed to.
    #[test]
    fn a_clip_inside_a_compound_maps_onto_the_outer_timeline() {
        let outer = Clip { source_in: 1.0, source_out: 3.0, timeline_start: 10.0, ..Clip::default() };
        // Runs 0.5..2.5 inside: its first half second is trimmed away.
        let inner = Clip { source_in: 5.0, source_out: 7.0, timeline_start: 0.5, ..Clip::default() };
        let mapped = map_into(&outer, &inner).unwrap();
        assert_eq!(mapped.timeline_start, 10.0);
        assert_eq!(mapped.source_in, 5.5);
        assert_eq!(mapped.timeline_end(), 11.5);
        // Entirely in the trimmed-off part: nothing.
        let early = Clip { source_out: 0.5, ..inner };
        let early = Clip { timeline_start: 0.0, ..early };
        assert!(map_into(&outer, &early).is_none());
    }

    #[test]
    fn a_compound_clips_sound_joins_the_mix() {
        let inner: Tracks = vec![
            (TrackKind::Video, vec![clip(0.0, 4.0)]),
            (TrackKind::Audio, vec![Clip { source: SourceId(3), ..clip(1.0, 2.0) }]),
        ];
        let compound = Clip { source: SourceId(9), ..clip(10.0, 4.0) };
        let own_audio = vec![Clip { source: SourceId(4), ..clip(0.0, 1.0) }];
        let tracks: Vec<(TrackKind, &[Clip])> = vec![
            (TrackKind::Video, std::slice::from_ref(&compound)),
            (TrackKind::Audio, own_audio.as_slice()),
        ];
        let flat = flatten_audio(&tracks, &|id| (id == SourceId(9)).then_some(inner.as_slice()));
        assert_eq!(flat.len(), 2);
        let all: Vec<&Clip> = flat.iter().flatten().collect();
        let from_compound = all.iter().find(|c| c.source == SourceId(3)).unwrap();
        assert_eq!(from_compound.timeline_start, 11.0);
        assert!(all.iter().any(|c| c.source == SourceId(4)));
    }

    #[test]
    fn a_track_moves_past_the_next_of_its_kind_the_way_it_is_drawn() {
        let mut tl = Timeline::new();
        for kind in [TrackKind::Video, TrackKind::Audio, TrackKind::Video, TrackKind::Audio] {
            tl.push_track(kind);
        }
        for (i, track) in tl.tracks.iter_mut().enumerate() {
            track.clips.push(Clip { id: i as u32, ..clip(0.0, 1.0) });
        }
        let ids = |tl: &Timeline| -> Vec<u32> { tl.tracks.iter().map(|t| t.clips[0].id).collect() };
        // Video 0 up the stack: past video 2, the audio in between untouched.
        assert_eq!(tl.move_track(0, true), Some(2));
        assert_eq!(ids(&tl), vec![2, 1, 0, 3]);
        assert_eq!(tl.move_track(2, true), None, "already on top");
        // Audio is drawn downward: up is the earlier audio track.
        assert_eq!(tl.move_track(3, true), Some(1));
        assert_eq!(ids(&tl), vec![2, 3, 0, 1]);
        assert_eq!(tl.move_track(1, true), None);
        assert_eq!(tl.move_track(1, false), Some(3));
    }

    #[test]
    fn a_ripple_carries_what_follows_and_stops_short_of_what_stays() {
        let mut tl = Timeline::new();
        tl.push_track(TrackKind::Video);
        tl.push_track(TrackKind::Audio);
        let freeze = Clip { id: tl.new_clip_id(), ..clip(4.0, 2.0) };
        let after = Clip { id: tl.new_clip_id(), ..clip(6.0, 4.0) };
        let before = Clip { id: tl.new_clip_id(), ..clip(0.0, 4.0) };
        // Music: one clip across the edit, which stays, and one after it
        // with a second's gap, which follows.
        let bed = Clip { id: tl.new_clip_id(), ..clip(0.0, 7.0) };
        let next = Clip { id: tl.new_clip_id(), ..clip(8.0, 2.0) };
        tl.tracks[0].clips = vec![before, freeze, after];
        tl.tracks[1].clips = vec![bed, next];

        let trimmed: HashSet<u32> = [freeze.id].into();
        let followers = tl.followers(freeze.timeline_end(), &trimmed);
        assert_eq!(followers, [after.id, next.id].into());
        // The freeze's own end goes with `after`, so only the music's gap
        // limits a shortening.
        assert_eq!(tl.ripple_room(&followers, &trimmed), 1.0);
        tl.shift_clips(&followers, 3.0);
        let (t, i) = tl.find(after.id).unwrap();
        assert_eq!(tl.tracks[t].clips[i].timeline_start, 9.0);
        let (t, i) = tl.find(next.id).unwrap();
        assert_eq!(tl.tracks[t].clips[i].timeline_start, 11.0);
    }

    #[test]
    fn a_gap_cuts_the_picked_clips_and_moves_everything_after_it() {
        let mut tl = Timeline::new();
        tl.push_track(TrackKind::Video);
        tl.push_track(TrackKind::Audio);
        tl.push_track(TrackKind::Audio);
        let link = tl.new_link_id();
        let shot = Clip { id: tl.new_clip_id(), link: Some(link), ..clip(0.0, 10.0) };
        let later = Clip { id: tl.new_clip_id(), ..clip(10.0, 5.0) };
        let sound = Clip { id: tl.new_clip_id(), link: Some(link), ..clip(0.0, 10.0) };
        let music = Clip { id: tl.new_clip_id(), ..clip(0.0, 20.0) };
        tl.tracks[0].clips = vec![shot, later];
        tl.tracks[1].clips = vec![sound];
        tl.tracks[2].clips = vec![music];

        tl.open_gap(4.0, 2.0, |c| c.link == Some(link));

        let spans = |ti: usize| -> Vec<(f64, f64, f64)> {
            tl.tracks[ti]
                .clips
                .iter()
                .map(|c| (c.timeline_start, c.timeline_end(), c.source_in))
                .collect()
        };
        assert_eq!(spans(0), vec![(0.0, 4.0, 0.0), (6.0, 12.0, 4.0), (12.0, 17.0, 0.0)]);
        assert_eq!(spans(1), vec![(0.0, 4.0, 0.0), (6.0, 12.0, 4.0)]);
        // Not picked, and across the cut: left playing under the gap.
        assert_eq!(spans(2), vec![(0.0, 20.0, 0.0)]);
        // The halves after the cut are still a linked pair, of their own.
        let right = tl.tracks[0].clips[1].link;
        assert!(right.is_some() && right != Some(link));
        assert_eq!(tl.tracks[1].clips[1].link, right);
    }

    fn board_of(tl: &Timeline, ids: &[u32]) -> Clipboard {
        tl.copy(&ids.iter().copied().collect())
    }

    #[test]
    fn a_paste_lands_at_the_playhead_keeping_its_spacing() {
        let mut tl = timeline_with(vec![clip(0.0, 2.0), clip(3.0, 1.0)]);
        let board = board_of(&tl, &[0, 1]);
        let ids = tl.paste(&board, 10.0, PasteLanes::default());
        assert_eq!(ids.len(), 2);
        let starts: Vec<f64> = ids.iter().map(|id| {
            let (t, i) = tl.find(*id).unwrap();
            tl.tracks[t].clips[i].timeline_start
        }).collect();
        assert_eq!(starts, vec![10.0, 13.0]);
    }

    #[test]
    fn a_paste_onto_a_clip_goes_after_it() {
        let mut tl = timeline_with(vec![clip(0.0, 4.0), clip(6.0, 2.0)]);
        let board = board_of(&tl, &[1]);
        // Playhead in the middle of the first clip: after it, where 2s fits.
        let ids = tl.paste(&board, 1.0, PasteLanes::default());
        let (t, i) = tl.find(ids[0]).unwrap();
        assert_eq!(tl.tracks[t].clips[i].timeline_start, 4.0);
        // Again at the same spot: the gap is now full up to 8, so after that.
        let ids = tl.paste(&board, 1.0, PasteLanes::default());
        let (t, i) = tl.find(ids[0]).unwrap();
        assert_eq!(tl.tracks[t].clips[i].timeline_start, 8.0);
    }

    #[test]
    fn pasted_clips_link_to_each_other_not_to_the_originals() {
        let mut tl = Timeline::new();
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks.push(Track::new(TrackKind::Audio));
        let link = tl.new_link_id();
        let (v, a) = (tl.new_clip_id(), tl.new_clip_id());
        tl.tracks[0].clips.push(Clip { id: v, link: Some(link), ..clip(0.0, 3.0) });
        tl.tracks[1].clips.push(Clip { id: a, link: Some(link), ..clip(0.0, 3.0) });
        let board = board_of(&tl, &[v, a]);
        let ids = tl.paste(&board, 5.0, PasteLanes::default());
        let (vt, vi) = tl.find(ids[0]).unwrap();
        let (at, ai) = tl.find(ids[1]).unwrap();
        assert_eq!((vt, at), (0, 1), "each back on its own track");
        let (nv, na) = (tl.tracks[vt].clips[vi], tl.tracks[at].clips[ai]);
        assert_eq!(nv.link, na.link);
        assert!(nv.link.is_some() && nv.link != Some(link));
        assert!(nv.id != v && na.id != a);
    }

    #[test]
    fn a_paste_whose_track_is_gone_makes_one() {
        let mut tl = Timeline::new();
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks.push(Track::new(TrackKind::Video));
        let id = tl.new_clip_id();
        tl.tracks[1].clips.push(Clip { id, ..clip(0.0, 1.0) });
        let board = board_of(&tl, &[id]);
        tl.tracks.truncate(1);
        let ids = tl.paste(&board, 0.0, PasteLanes::default());
        assert_eq!(tl.tracks.len(), 2);
        assert_eq!(tl.find(ids[0]).unwrap().0, 1);
        assert_eq!(tl.tracks[1].kind, TrackKind::Video);
    }

    /// A link switched off is still there to switch back on, on both halves,
    /// and stops counting as a link in between.
    #[test]
    fn a_link_switched_off_keeps_its_partner() {
        let mut tl = Timeline::new();
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks.push(Track::new(TrackKind::Audio));
        let link = tl.new_link_id();
        let (v, a, other) = (tl.new_clip_id(), tl.new_clip_id(), tl.new_clip_id());
        tl.tracks[0].clips.push(Clip { id: v, link: Some(link), ..clip(0.0, 3.0) });
        tl.tracks[1].clips.push(Clip { id: a, link: Some(link), ..clip(0.0, 3.0) });
        tl.tracks[0].clips.push(Clip { id: other, ..clip(3.0, 1.0) });
        tl.set_unlinked(link, true);
        for track in 0..2 {
            let half = tl.tracks[track].clips[0];
            assert_eq!(half.live_link(), None);
            assert!(tl.has_partner(&half));
        }
        assert!(!tl.has_partner(&tl.tracks[0].clips[1]));
        // With the sound deleted there is no pair left to put back together.
        tl.tracks[1].clips.clear();
        assert!(!tl.has_partner(&tl.tracks[0].clips[0]));
        tl.set_unlinked(link, false);
        assert_eq!(tl.tracks[0].clips[0].live_link(), Some(link));
    }

    /// The point of a target lane: the copy lands on the track picked, not the
    /// one it was made on, and linked audio with no target of its own stays
    /// where it was.
    #[test]
    fn a_paste_goes_onto_the_lane_it_is_aimed_at() {
        let mut tl = Timeline::new();
        for kind in [TrackKind::Video, TrackKind::Audio, TrackKind::Video] {
            tl.tracks.push(Track::new(kind));
        }
        let link = tl.new_link_id();
        let (v, a) = (tl.new_clip_id(), tl.new_clip_id());
        tl.tracks[0].clips.push(Clip { id: v, link: Some(link), ..clip(0.0, 3.0) });
        tl.tracks[1].clips.push(Clip { id: a, link: Some(link), ..clip(0.0, 3.0) });
        let board = board_of(&tl, &[v, a]);
        // V2 is the third track: lanes count within a kind.
        let onto = PasteLanes { video: Some(1), audio: None };
        let ids = tl.paste(&board, 0.0, onto);
        assert_eq!(tl.find(ids[0]).unwrap().0, 2);
        assert_eq!(tl.find(ids[1]).unwrap().0, 1);
        // The audio had its original in the way, and the pair moved as one.
        for id in ids {
            let (t, i) = tl.find(id).unwrap();
            assert_eq!(tl.tracks[t].clips[i].timeline_start, 3.0);
        }
    }

    /// Clips from several lanes keep their stacking: the lowest on the target,
    /// the rest as far above it as they were, on new tracks where the
    /// timeline runs out.
    #[test]
    fn a_stack_pasted_onto_a_lane_keeps_its_order() {
        let mut tl = Timeline::new();
        for _ in 0..3 {
            tl.tracks.push(Track::new(TrackKind::Video));
        }
        let (low, high) = (tl.new_clip_id(), tl.new_clip_id());
        tl.tracks[0].clips.push(Clip { id: low, ..clip(0.0, 1.0) });
        tl.tracks[2].clips.push(Clip { id: high, ..clip(0.0, 1.0) });
        let board = board_of(&tl, &[low, high]);
        let onto = PasteLanes { video: Some(1), audio: None };
        let ids = tl.paste(&board, 0.0, onto);
        // V1 and V3 onto V2: V2, and a V4 that had to be made.
        assert_eq!(tl.find(ids[0]).unwrap().0, 1);
        assert_eq!(tl.find(ids[1]).unwrap().0, 3);
        assert_eq!(tl.tracks.len(), 4);
    }

    /// Only the clips picked are cut: the rest of the stack under the playhead
    /// is left whole.
    #[test]
    fn a_split_cuts_only_the_clips_it_is_given() {
        let mut tl = Timeline::new();
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks.push(Track::new(TrackKind::Video));
        tl.tracks[0].clips = vec![Clip { id: 1, source_out: 10.0, ..Clip::default() }];
        tl.tracks[1].clips = vec![Clip { id: 2, source_out: 10.0, ..Clip::default() }];
        tl.next_clip = 3;
        tl.split_at(4.0, |c| c.id == 2);
        assert_eq!(tl.tracks[0].clips.len(), 1);
        assert_eq!(tl.tracks[1].clips.len(), 2);
    }

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

        tl.split_at(4.0, |_| true);
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

        tl.split_at(4.0, |_| true);
        let first = tl.tracks[0].clips[1].link;

        tl.restore(&before);
        tl.split_at(4.0, |_| true);
        // The rewind is what keeps ids from drifting upward on every
        // undo/redo cycle; the clip that held the old id is gone.
        assert_eq!(tl.tracks[0].clips[1].link, first);
    }

    #[test]
    fn a_split_keeps_the_left_half_s_name_and_coins_one_for_the_right() {
        let mut tl = timeline_with(vec![clip(0.0, 10.0)]);
        let original = tl.tracks[0].clips[0].id;

        tl.split_at(4.0, |_| true);
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

        tl.split_at(4.0, |_| true);
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

        tl.split_at(4.0, |_| true);
        let first = tl.tracks[0].clips[1].id;

        tl.restore(&before);
        tl.split_at(4.0, |_| true);
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
        tl.split_at(4.0, |_| true);
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
        tl.split_at(8.0, |_| true);
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
