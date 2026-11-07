use std::collections::HashMap;

use ffmpeg_next as ffmpeg;

use crate::audio::{self, AudioStream, Waveform};
use crate::filmstrip::Filmstrip;
use crate::still::{self, Still};
use crate::quad::{QuadRenderer, Texture};
use crate::timeline::{tracks_duration, Clip, SourceId, TrackKind, Tracks};
use crate::title::{self, Title};
use crate::video::VideoStream;

pub struct Source {
    /// `None` for a source with no picture — a music bed or a voiceover.
    /// The pool holds those too: an edit needs them on its audio tracks, and
    /// requiring a video stream to import meant a `.wav` could not be brought
    /// in at all.
    pub stream: Option<VideoStream>,
    pub audio: Option<AudioStream>,
    pub waveform: Option<Waveform>,
    /// `Some` for an image: one picture, decoded once, with no length of its
    /// own. Never alongside `stream`.
    pub still: Option<Still>,
    /// Frames across the clip for scrubbing its pool thumbnail. `None` for a
    /// source with no picture to scrub.
    pub filmstrip: Option<Filmstrip>,
    pub name: String,
    /// Kept so an export can open its own decoders from the original file
    /// rather than borrowing the ones driving the preview.
    ///
    /// Empty for a generated source, which has no file to reopen.
    pub path: String,
    /// `Some` for a title, whose picture is drawn rather than decoded.
    pub title: Option<Title>,
    /// `Some` for a compound clip: clips of other sources, on tracks of their
    /// own, that play as one.
    pub compound: Option<Tracks>,
    /// The title's picture, baked for one canvas size and one state of the
    /// text. Rebuilt when either changes and otherwise reused, so typing into
    /// a title costs one rasterization per keystroke rather than one per frame.
    title_texture: Option<Texture>,
    title_baked: Option<(u64, u32, u32)>,
    /// Bumped whenever the title changes. Comparing a counter beats comparing
    /// the text itself, which would mean a string compare on every frame to
    /// discover that nothing had happened.
    revision: u64,
}

impl Source {
    /// A source with nothing in it yet but a name, for the constructors to
    /// fill in the one kind of picture or sound they have.
    fn empty(name: String, path: String) -> Self {
        Self {
            stream: None,
            audio: None,
            waveform: None,
            still: None,
            filmstrip: None,
            name,
            path,
            title: None,
            compound: None,
            title_texture: None,
            title_baked: None,
            revision: 0,
        }
    }

    /// The title's baked picture, if one has been built for the size being
    /// drawn. `None` for footage, and for a title whose bake has not run yet —
    /// see [`MediaPool::bake_title`].
    pub fn title_texture(&self) -> Option<&Texture> {
        self.title_texture.as_ref()
    }
}

pub struct MediaPool {
    sources: HashMap<SourceId, Source>,
    order: Vec<SourceId>,
    next_id: u32,
}

impl MediaPool {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            order: Vec::new(),
            next_id: 0,
        }
    }

    pub fn add(
        &mut self,
        path: &str,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) -> Result<SourceId, ffmpeg::Error> {
        if still::is_still(path) {
            let still = Still::open(path, device, queue, quads)?;
            return Ok(self.insert(Source {
                still: Some(still),
                ..Source::empty(file_name(path), path.to_string())
            }));
        }
        // Neither stream is required on its own — only that the file yields at
        // least one of them. A video-only file imports without audio, an
        // audio-only file imports without a picture, and a file that gives
        // neither is the one that failed to open, so it reports the video
        // error rather than a vaguer one of its own.
        let video = VideoStream::open(path, device, queue, quads);
        let audio = match AudioStream::open(path) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("skipping audio for {path}: {e}");
                None
            }
        };
        let stream = match video {
            Ok(v) => Some(v),
            Err(e) => {
                if audio.is_none() {
                    return Err(e);
                }
                log::info!("{path} has no video stream; importing as audio only");
                None
            }
        };
        // Build a peak summary up front for the timeline waveform. Failure
        // here just means the audio clip renders as a flat rect — not fatal.
        let waveform = if audio.is_some() {
            match audio::build_waveform(path) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("waveform build failed for {path}: {e}");
                    None
                }
            }
        } else {
            None
        };
        let filmstrip = stream.as_ref().map(|v| Filmstrip::spawn(path, v.duration()));
        Ok(self.insert(Source {
            stream,
            audio,
            waveform,
            filmstrip,
            ..Source::empty(file_name(path), path.to_string())
        }))
    }

    fn insert(&mut self, source: Source) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id += 1;
        self.sources.insert(id, source);
        self.order.push(id);
        id
    }

    /// Add a generated title to the pool. Cannot fail — there is no file to
    /// open, which is rather the point of one.
    pub fn add_title(&mut self, title: Title) -> SourceId {
        let name = title.pool_name().to_string();
        self.insert(Source { title: Some(title), ..Source::empty(name, String::new()) })
    }

    /// Add a compound clip holding `tracks`, named `name`.
    pub fn add_compound(&mut self, name: String, tracks: Tracks) -> SourceId {
        self.insert(Source { compound: Some(tracks), ..Source::empty(name, String::new()) })
    }

    /// What the compound clip `id` holds, or `None` for any other source.
    pub fn compound(&self, id: SourceId) -> Option<&[(TrackKind, Vec<Clip>)]> {
        self.sources.get(&id)?.compound.as_deref()
    }

    pub fn set_compound(&mut self, id: SourceId, tracks: Tracks) {
        if let Some(src) = self.sources.get_mut(&id) {
            if src.compound.is_some() {
                src.compound = Some(tracks);
            }
        }
    }

    /// Every compound clip's contents, for the undo history, in id order for
    /// the same reason as [`MediaPool::title_snapshot`].
    pub fn compound_snapshot(&self) -> Vec<(SourceId, Tracks)> {
        let mut all: Vec<(SourceId, Tracks)> = self
            .sources
            .iter()
            .filter_map(|(id, src)| src.compound.clone().map(|c| (*id, c)))
            .collect();
        all.sort_by_key(|(id, _)| id.0);
        all
    }

    pub fn restore_compounds(&mut self, all: &[(SourceId, Tracks)]) {
        for (id, tracks) in all {
            self.set_compound(*id, tracks.clone());
        }
    }

    /// Take every clip of `source` out of every compound clip, as removing it
    /// from the pool takes it off the timeline.
    pub fn remove_from_compounds(&mut self, source: SourceId) {
        for src in self.sources.values_mut() {
            if let Some(tracks) = src.compound.as_mut() {
                for (_, clips) in tracks {
                    clips.retain(|c| c.source != source);
                }
            }
        }
    }

    /// Whether `outer` is `inner` or holds it, however deep: what would make
    /// a compound clip contain itself.
    pub fn contains_compound(&self, outer: SourceId, inner: SourceId) -> bool {
        self.contains_at(outer, inner, 0)
    }

    fn contains_at(&self, outer: SourceId, inner: SourceId, depth: usize) -> bool {
        if outer == inner {
            return true;
        }
        if depth >= crate::timeline::MAX_NESTING {
            return true;
        }
        self.compound(outer).is_some_and(|tracks| {
            tracks
                .iter()
                .flat_map(|(_, clips)| clips.iter())
                .any(|c| self.contains_at(c.source, inner, depth + 1))
        })
    }

    pub fn title(&self, id: SourceId) -> Option<&Title> {
        self.sources.get(&id)?.title.as_ref()
    }

    /// Replace a title's contents, invalidating the picture baked from the old
    /// ones. The only way to change a title, so a stale bake cannot outlive an
    /// edit.
    pub fn set_title(&mut self, id: SourceId, title: Title) {
        let Some(src) = self.sources.get_mut(&id) else {
            return;
        };
        if src.title.as_ref() == Some(&title) {
            return;
        }
        src.name = title.pool_name().to_string();
        src.title = Some(title);
        src.revision += 1;
    }

    /// Every title in the pool, for the undo history — which has to be able to
    /// put back what a title said as well as which clips referred to it.
    pub fn title_snapshot(&self) -> Vec<(SourceId, Title)> {
        let mut titles: Vec<(SourceId, Title)> = self
            .sources
            .iter()
            .filter_map(|(id, src)| src.title.clone().map(|t| (*id, t)))
            .collect();
        // A `HashMap` hands them over in whatever order it likes, and two
        // snapshots that differ only in that order would read as a real edit.
        titles.sort_by_key(|(id, _)| id.0);
        titles
    }

    pub fn restore_titles(&mut self, titles: &[(SourceId, Title)]) {
        for (id, title) in titles {
            self.set_title(*id, title.clone());
        }
    }

    /// Make sure `id`'s title has a picture baked at `width x height`, ready
    /// for the preview to draw.
    ///
    /// Split out from drawing because it needs the pool mutably and the GPU
    /// immutably, which is the one combination the render pass cannot hold
    /// while it is pushing quads.
    pub fn bake_title(
        &mut self,
        id: SourceId,
        canvas: (u32, u32),
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) {
        let (width, height) = canvas;
        let Some(src) = self.sources.get_mut(&id) else {
            return;
        };
        let Some(title) = src.title.as_ref() else {
            return;
        };
        let key = (src.revision, width, height);
        if src.title_baked == Some(key) {
            return;
        }
        // The same picture the export draws, so what is styled here is what
        // lands in the file.
        let picture = title::render(title, width, height);
        let texture = quads.create_empty_texture(
            device,
            width,
            height,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        texture.write_region(queue, 0, 0, width, height, &picture.pixels);
        src.title_texture = Some(texture);
        src.title_baked = Some(key);
    }

    /// Upload any hover filmstrips whose decode threads have finished.
    pub fn poll_filmstrips(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) {
        for src in self.sources.values_mut() {
            if let Some(strip) = src.filmstrip.as_mut() {
                strip.poll(device, queue, quads);
            }
        }
    }

    /// Let every stream that has gone off screen give back the frames it was
    /// keeping for stepping backwards. See [`VideoStream::release_idle`].
    pub fn release_idle_frames(&mut self) {
        for src in self.sources.values_mut() {
            if let Some(stream) = src.stream.as_mut() {
                stream.release_idle();
            }
        }
    }

    /// Drop `id` from the visible pool list. The decoded `Source` — GPU
    /// textures, audio, waveform — is deliberately retained so undo can bring
    /// the row back instantly instead of re-opening and re-scanning the file.
    /// Nothing outside `order` is reachable from the UI, so a hidden source is
    /// inert until `set_order` restores it.
    pub fn remove(&mut self, id: SourceId) {
        self.order.retain(|x| *x != id);
    }

    /// Restore a previously snapshotted pool order. Ids with no backing source
    /// are skipped rather than trusted, so a stale snapshot can't resurrect a
    /// row that would render as a blank entry.
    pub fn set_order(&mut self, order: &[SourceId]) {
        self.order = order
            .iter()
            .copied()
            .filter(|id| self.sources.contains_key(id))
            .collect();
    }

    /// The source opened from the file at `path`, shown in the pool or not.
    /// Paths are compared as the files they name, so the same file reached
    /// through a symlink or a `..` is still the same file.
    pub fn find_path(&self, path: &str) -> Option<SourceId> {
        let same = |a: &str| {
            a == path
                || matches!(
                    (std::fs::canonicalize(a), std::fs::canonicalize(path)),
                    (Ok(x), Ok(y)) if x == y
                )
        };
        self.all_ids()
            .into_iter()
            .find(|id| self.sources.get(id).is_some_and(|s| !s.path.is_empty() && same(&s.path)))
    }

    /// Whether `id` is listed in the pool, rather than held back for undo.
    pub fn is_shown(&self, id: SourceId) -> bool {
        self.order.contains(&id)
    }

    /// List `id` in the pool again, at the end, after a removal hid it.
    pub fn show(&mut self, id: SourceId) {
        if self.sources.contains_key(&id) && !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    pub fn get(&self, id: SourceId) -> Option<&Source> {
        self.sources.get(&id)
    }

    pub fn get_mut(&mut self, id: SourceId) -> Option<&mut Source> {
        self.sources.get_mut(&id)
    }

    /// Longest a clip of this source can be trimmed to. Video length where
    /// there is a picture, the audio's otherwise — and for a title, which
    /// nothing runs out of, a bound rather than a length.
    pub fn duration(&self, id: SourceId) -> f64 {
        self.sources.get(&id).map_or(0.0, |s| {
            if s.title.is_some() {
                return title::MAX_DURATION;
            }
            if s.still.is_some() {
                return still::MAX_DURATION;
            }
            if let Some(tracks) = s.compound.as_ref() {
                return tracks_duration(tracks);
            }
            s.stream
                .as_ref()
                .map(|v| v.duration())
                .or_else(|| s.audio.as_ref().map(|a| a.duration()))
                .unwrap_or(0.0)
        })
    }

    /// How long a clip dropped from this row starts out.
    ///
    /// The whole of a file, since that is what "this footage" means. A title
    /// has no such length, so it gets a workable default to trim from — the
    /// alternative is dropping an hour-long clip across the timeline.
    pub fn drop_duration(&self, id: SourceId) -> f64 {
        match self.sources.get(&id) {
            Some(s) if s.title.is_some() => return title::DEFAULT_DURATION,
            Some(s) if s.still.is_some() => return still::DEFAULT_DURATION,
            _ => {}
        }
        self.duration(id)
    }

    /// Whether a clip of this source has anything to show on a video track.
    pub fn has_video(&self, id: SourceId) -> bool {
        self.sources
            .get(&id)
            .is_some_and(|s| {
                s.stream.is_some() || s.title.is_some() || s.still.is_some() || s.compound.is_some()
            })
    }

    pub fn audio_duration(&self, id: SourceId) -> Option<f64> {
        self.sources
            .get(&id)
            .and_then(|s| s.audio.as_ref().map(|a| a.duration()))
    }

    pub fn ids(&self) -> &[SourceId] {
        &self.order
    }

    /// Every source held, shown in the pool or not — removed ones are kept
    /// for undo, and can be found again rather than opened a second time.
    pub fn all_ids(&self) -> Vec<SourceId> {
        let mut ids: Vec<SourceId> = self.sources.keys().copied().collect();
        ids.sort_by_key(|id| id.0);
        ids
    }
}

/// The file name a pool row shows for `path`.
fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_found_by_any_path_to_it_shown_or_not() {
        let dir = std::env::temp_dir().join(format!("ruve_find_path_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        std::fs::write(&file, b"x").unwrap();
        let link = dir.join("link.mp4");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let path = file.to_str().unwrap();

        let mut pool = MediaPool::new();
        pool.add_title(Title::default());
        let id = pool.insert(Source::empty("clip.mp4".into(), path.to_string()));
        assert_eq!(pool.find_path(path), Some(id));
        assert_eq!(pool.find_path(link.to_str().unwrap()), Some(id));
        let dotted = dir.join("..").join(dir.file_name().unwrap()).join("clip.mp4");
        assert_eq!(pool.find_path(dotted.to_str().unwrap()), Some(id));
        assert_eq!(pool.find_path(dir.join("other.mp4").to_str().unwrap()), None);

        // Removed, it is still found, and showing it lists it once.
        pool.remove(id);
        assert!(!pool.is_shown(id));
        assert_eq!(pool.find_path(path), Some(id));
        pool.show(id);
        pool.show(id);
        assert_eq!(pool.ids().iter().filter(|x| **x == id).count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
