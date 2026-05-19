use std::collections::HashMap;

use ffmpeg_next as ffmpeg;

use crate::audio::{self, AudioStream, Waveform};
use crate::quad::QuadRenderer;
use crate::timeline::SourceId;
use crate::video::VideoStream;

pub struct Source {
    pub stream: VideoStream,
    pub audio: Option<AudioStream>,
    pub waveform: Option<Waveform>,
    pub name: String,
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
        let stream = VideoStream::open(path, device, queue, quads)?;
        // Missing/undecodable audio shouldn't block importing a video-only file.
        let audio = match AudioStream::open(path) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("skipping audio for {path}: {e}");
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
            },
        );
        self.order.push(id);
        Ok(id)
    }

    pub fn remove(&mut self, id: SourceId) {
        self.sources.remove(&id);
        self.order.retain(|x| *x != id);
    }

    pub fn get(&self, id: SourceId) -> Option<&Source> {
        self.sources.get(&id)
    }

    pub fn get_mut(&mut self, id: SourceId) -> Option<&mut Source> {
        self.sources.get_mut(&id)
    }

    pub fn duration(&self, id: SourceId) -> f64 {
        self.sources.get(&id).map_or(0.0, |s| s.stream.duration())
    }

    pub fn has_audio(&self, id: SourceId) -> bool {
        self.sources
            .get(&id)
            .map_or(false, |s| s.audio.is_some())
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
