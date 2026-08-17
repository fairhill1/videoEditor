use std::collections::HashMap;

use ffmpeg_next as ffmpeg;

use crate::audio::{self, AudioStream, Waveform};
use crate::quad::{QuadRenderer, Texture};
use crate::timeline::SourceId;
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
    pub name: String,
    /// Kept so an export can open its own decoders from the original file
    /// rather than borrowing the ones driving the preview.
    ///
    /// Empty for a generated source, which has no file to reopen.
    pub path: String,
    /// `Some` for a title, whose picture is drawn rather than decoded.
    pub title: Option<Title>,
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
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let id = SourceId(self.next_id);
        self.next_id += 1;
        self.sources.insert(
            id,
            Source {
                stream,
                audio,
                waveform,
                name,
                path: path.to_string(),
                title: None,
                title_texture: None,
                title_baked: None,
                revision: 0,
            },
        );
        self.order.push(id);
        Ok(id)
    }

    /// Add a generated title to the pool. Cannot fail — there is no file to
    /// open, which is rather the point of one.
    pub fn add_title(&mut self, title: Title) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id += 1;
        self.sources.insert(
            id,
            Source {
                stream: None,
                audio: None,
                waveform: None,
                name: title.pool_name().to_string(),
                path: String::new(),
                title: Some(title),
                title_texture: None,
                title_baked: None,
                revision: 0,
            },
        );
        self.order.push(id);
        id
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
    /// `caret` adds a bar after the last character, for the title being typed
    /// into. Part of the picture rather than something drawn over it, because
    /// the text is centred: only the rasterizer knows where the last character
    /// ended up, and it moves with every keystroke.
    pub fn bake_title(
        &mut self,
        id: SourceId,
        canvas: (u32, u32),
        caret: bool,
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
        // The caret joins the key, so putting it up or taking it away rebakes
        // exactly once rather than not at all.
        let key = (src.revision * 2 + caret as u64, width, height);
        if src.title_baked == Some(key) {
            return;
        }
        let shown;
        let title = if caret {
            shown = Title {
                text: format!("{}{}", title.text, title::CARET),
                ..title.clone()
            };
            &shown
        } else {
            title
        };
        let mask = title::rasterize(title, width, height);
        // White with the coverage in alpha, exactly as the glyph atlas stores
        // its own bitmaps: the colour rides on the quad instead, so a title
        // recoloured does not have to be rasterized again.
        let mut rgba = vec![255u8; mask.coverage.len() * 4];
        for (i, &c) in mask.coverage.iter().enumerate() {
            rgba[i * 4 + 3] = c;
        }
        let texture = quads.create_empty_texture(
            device,
            width,
            height,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        texture.write_region(queue, 0, 0, width, height, &rgba);
        src.title_texture = Some(texture);
        src.title_baked = Some(key);
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
        if self.sources.get(&id).is_some_and(|s| s.title.is_some()) {
            return title::DEFAULT_DURATION;
        }
        self.duration(id)
    }

    /// Whether a clip of this source has anything to show on a video track.
    pub fn has_video(&self, id: SourceId) -> bool {
        self.sources
            .get(&id)
            .is_some_and(|s| s.stream.is_some() || s.title.is_some())
    }

    pub fn audio_duration(&self, id: SourceId) -> Option<f64> {
        self.sources
            .get(&id)
            .and_then(|s| s.audio.as_ref().map(|a| a.duration()))
    }

    pub fn ids(&self) -> &[SourceId] {
        &self.order
    }
}
