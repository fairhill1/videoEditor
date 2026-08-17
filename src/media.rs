use std::collections::HashMap;

use ffmpeg_next as ffmpeg;

use crate::audio::{self, AudioStream, Waveform};
use crate::quad::QuadRenderer;
use crate::timeline::SourceId;
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
    pub path: String,
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
            },
        );
        self.order.push(id);
        Ok(id)
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

    /// Longest thing the source has to play. Video length where there is a
    /// picture, the audio's otherwise — it is what a clip dropped from this
    /// row is sized to, and an audio-only row that reported zero would drop as
    /// a clip with no duration at all.
    pub fn duration(&self, id: SourceId) -> f64 {
        self.sources.get(&id).map_or(0.0, |s| {
            s.stream
                .as_ref()
                .map(|v| v.duration())
                .or_else(|| s.audio.as_ref().map(|a| a.duration()))
                .unwrap_or(0.0)
        })
    }

    pub fn has_video(&self, id: SourceId) -> bool {
        self.sources
            .get(&id)
            .is_some_and(|s| s.stream.is_some())
    }

    pub fn has_audio(&self, id: SourceId) -> bool {
        self.sources.get(&id).is_some_and(|s| s.audio.is_some())
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
