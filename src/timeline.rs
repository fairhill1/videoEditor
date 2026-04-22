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
}

impl Timeline {
    pub fn new() -> Self {
        Self { tracks: Vec::new() }
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
    pub fn split_at(&mut self, t: f64) {
        for track in &mut self.tracks {
            let mut i = 0;
            while i < track.clips.len() {
                let orig = track.clips[i];
                if orig.contains(t) && t > orig.timeline_start {
                    let split_source_t = orig.source_time(t);
                    track.clips[i].source_out = split_source_t;
                    track.clips.insert(
                        i + 1,
                        Clip {
                            source: orig.source,
                            source_in: split_source_t,
                            source_out: orig.source_out,
                            timeline_start: t,
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
