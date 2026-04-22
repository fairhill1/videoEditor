use std::collections::HashMap;

use ffmpeg_next as ffmpeg;

use crate::quad::QuadRenderer;
use crate::timeline::SourceId;
use crate::video::VideoStream;

pub struct Source {
    pub stream: VideoStream,
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
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let id = SourceId(self.next_id);
        self.next_id += 1;
        self.sources.insert(id, Source { stream, name });
        self.order.push(id);
        Ok(id)
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

    pub fn ids(&self) -> &[SourceId] {
        &self.order
    }
}
