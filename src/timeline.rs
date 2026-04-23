#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Copy, Clone, Debug)]
pub struct Clip {
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
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_link: 0,
        }
    }

    /// Allocate a fresh link id. Call this when establishing a new linked
    /// group (e.g. auto-pairing a video drop with its audio clip).
    pub fn new_link_id(&mut self) -> u32 {
        let id = self.next_link;
        self.next_link += 1;
        id
    }

    pub fn duration(&self) -> f64 {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(|c| c.timeline_end()))
            .fold(0.0_f64, f64::max)
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

        for track in &mut self.tracks {
            let mut i = 0;
            while i < track.clips.len() {
                let orig = track.clips[i];
                if orig.contains(t) && t > orig.timeline_start {
                    let split_source_t = orig.source_time(t);
                    track.clips[i].source_out = split_source_t;
                    let right_link = orig.link.map(|old| relink[&old]);
                    track.clips.insert(
                        i + 1,
                        Clip {
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
    }
}
