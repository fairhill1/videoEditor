use ffmpeg_next as ffmpeg;

use crate::quad::{QuadRenderer, Texture};

const AV_TIME_BASE: f64 = 1_000_000.0;

pub struct VideoStream {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    stream_index: usize,
    time_base_seconds: f64,
    duration_seconds: f64,
    width: u32,
    height: u32,
    texture: Texture,
    pending: Option<(Vec<u8>, f64)>,
}

impl VideoStream {
    pub fn open(
        path: &str,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) -> Result<Self, ffmpeg::Error> {
        ffmpeg::init()?;

        let ictx = ffmpeg::format::input(&path)?;

        let duration_raw = ictx.duration();
        let duration_seconds = if duration_raw > 0 {
            duration_raw as f64 / AV_TIME_BASE
        } else {
            0.0
        };

        let (stream_index, time_base_seconds, parameters) = {
            let input = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .ok_or(ffmpeg::Error::StreamNotFound)?;
            let tb = input.time_base();
            (
                input.index(),
                tb.numerator() as f64 / tb.denominator() as f64,
                input.parameters(),
            )
        };

        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
        let decoder = ctx.decoder().video()?;

        let width = decoder.width();
        let height = decoder.height();

        let scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;

        let texture =
            quads.create_empty_texture(device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb);

        let mut stream = Self {
            ictx,
            decoder,
            scaler,
            stream_index,
            time_base_seconds,
            duration_seconds,
            width,
            height,
            texture,
            pending: None,
        };

        // Prime with the first frame so nothing flashes black at startup.
        if let Some((rgba, _pts)) = stream.decode_next() {
            stream
                .texture
                .write_region(queue, 0, 0, width, height, &rgba);
        }

        Ok(stream)
    }

    pub fn texture(&self) -> &Texture {
        &self.texture
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn duration(&self) -> f64 {
        self.duration_seconds
    }

    /// Decode and upload frames up to time `t` (in seconds from start of video).
    pub fn advance_to(&mut self, queue: &wgpu::Queue, t: f64) {
        loop {
            if self.pending.is_none() {
                match self.decode_next() {
                    Some(p) => self.pending = Some(p),
                    None => return,
                }
            }

            let pts = self.pending.as_ref().unwrap().1;
            if pts > t {
                return;
            }

            let (rgba, _) = self.pending.take().unwrap();
            self.texture
                .write_region(queue, 0, 0, self.width, self.height, &rgba);
        }
    }

    pub fn seek(&mut self, t: f64) {
        let ts = (t.max(0.0) * AV_TIME_BASE) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        self.pending = None;
    }

    fn decode_next(&mut self) -> Option<(Vec<u8>, f64)> {
        let mut frame = ffmpeg::frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let pts = frame.pts().unwrap_or(0);
                    let pts_sec = pts as f64 * self.time_base_seconds;

                    let mut rgba_frame = ffmpeg::frame::Video::empty();
                    self.scaler.run(&frame, &mut rgba_frame).ok()?;

                    let stride = rgba_frame.stride(0);
                    let row_bytes = (self.width * 4) as usize;
                    let src = rgba_frame.data(0);
                    let mut rgba = vec![0u8; row_bytes * self.height as usize];
                    for y in 0..self.height as usize {
                        let src_off = y * stride;
                        let dst_off = y * row_bytes;
                        rgba[dst_off..dst_off + row_bytes]
                            .copy_from_slice(&src[src_off..src_off + row_bytes]);
                    }
                    return Some((rgba, pts_sec));
                }
                Err(_) => match self.next_video_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => return None,
                },
            }
        }
    }

    fn next_video_packet(&mut self) -> Option<ffmpeg::Packet> {
        let mut iter = self.ictx.packets();
        loop {
            let (stream, packet) = iter.next()?;
            if stream.index() == self.stream_index {
                return Some(packet);
            }
        }
    }
}
