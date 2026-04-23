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
    frame_rate: f64,
    texture: Texture,
    thumbnail: Texture,
    pending: Option<(ffmpeg::frame::Video, f64)>,
    rgba_buf: Vec<u8>,
    scaled: ffmpeg::frame::Video,
    displayed_pts: Option<f64>,
}

/// Short side of the baked media-pool thumbnail. The pool slot is a fixed
/// aspect ratio, so the UI letterboxes the thumb into its slot — we only need
/// enough pixels here to look crisp at typical row sizes.
const THUMB_HEIGHT: u32 = 64;

/// How far forward `t` can be from the displayed frame before `goto` gives up
/// on linear decode and issues a seek. Tuned to cover a normal playback step
/// (well under one frame at 24fps is ~0.04s; a 1s buffer absorbs render stalls
/// without making scrubs decode through the whole file).
const FORWARD_DECODE_BUDGET: f64 = 1.0;

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

        let (stream_index, time_base_seconds, parameters, frame_rate) = {
            let input = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .ok_or(ffmpeg::Error::StreamNotFound)?;
            let tb = input.time_base();
            let afr = input.avg_frame_rate();
            let fr = {
                let num = afr.numerator() as f64;
                let den = afr.denominator() as f64;
                if num > 0.0 && den > 0.0 { num / den } else { 30.0 }
            };
            (
                input.index(),
                tb.numerator() as f64 / tb.denominator() as f64,
                input.parameters(),
                fr,
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

        let thumb_h = THUMB_HEIGHT.min(height).max(1);
        let thumb_w =
            ((thumb_h as f64 * width as f64 / height as f64).round() as u32).max(1);
        let mut thumb_scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            thumb_w,
            thumb_h,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;
        let thumbnail = quads.create_empty_texture(
            device,
            thumb_w,
            thumb_h,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );

        let mut stream = Self {
            ictx,
            decoder,
            scaler,
            stream_index,
            time_base_seconds,
            duration_seconds,
            width,
            height,
            frame_rate,
            texture,
            thumbnail,
            pending: None,
            rgba_buf: vec![0u8; (width * height * 4) as usize],
            scaled: ffmpeg::frame::Video::empty(),
            displayed_pts: None,
        };

        // Prime with the first frame so nothing flashes black at startup.
        // Bake the pool thumbnail from the same frame — it's already decoded
        // and we only need it once, so reusing it avoids a second seek/decode.
        if let Some((frame, pts)) = stream.decode_next_raw() {
            bake_thumbnail(
                queue,
                &mut thumb_scaler,
                &frame,
                thumb_w,
                thumb_h,
                &stream.thumbnail,
            );
            stream.scale_and_upload(queue, &frame);
            stream.displayed_pts = Some(pts);
        }

        Ok(stream)
    }

    /// Display the frame at `t` using whichever strategy is cheapest.
    /// - `t` went backward, or is far beyond the decoded position: seek.
    /// - `t` is close ahead: let the decoder walk forward (cheap).
    /// This is the only entry point callers should use during playback/scrub;
    /// it keeps per-frame decoder work bounded even under rapid seek requests.
    pub fn goto(&mut self, queue: &wgpu::Queue, t: f64) {
        match self.displayed_pts {
            Some(d) if t < d || t > d + FORWARD_DECODE_BUDGET => self.seek(queue, t),
            Some(_) => self.advance_to(queue, t),
            None => self.seek(queue, t),
        }
    }

    pub fn texture(&self) -> &Texture {
        &self.texture
    }

    pub fn thumbnail(&self) -> &Texture {
        &self.thumbnail
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

    pub fn frame_rate(&self) -> f64 {
        self.frame_rate
    }

    /// Decode frames up to time `t` (seconds). Only the latest frame with `pts <= t`
    /// is scaled and uploaded; intermediate frames are decoded-then-discarded so
    /// H.264/VP9/etc temporal deps stay intact without paying for N scales+uploads.
    pub fn advance_to(&mut self, queue: &wgpu::Queue, t: f64) {
        if self.pending.is_none() {
            match self.decode_next_raw() {
                Some(p) => self.pending = Some(p),
                None => return,
            }
        }

        if self.pending.as_ref().unwrap().1 > t {
            return;
        }

        loop {
            match self.decode_next_raw() {
                Some((next_frame, next_pts)) => {
                    if next_pts <= t {
                        // Drop the current pending — we'll never display it.
                        self.pending = Some((next_frame, next_pts));
                    } else {
                        let (cur_frame, cur_pts) = self.pending.take().unwrap();
                        self.scale_and_upload(queue, &cur_frame);
                        self.displayed_pts = Some(cur_pts);
                        self.pending = Some((next_frame, next_pts));
                        return;
                    }
                }
                None => {
                    if let Some((cur_frame, cur_pts)) = self.pending.take() {
                        self.scale_and_upload(queue, &cur_frame);
                        self.displayed_pts = Some(cur_pts);
                    }
                    return;
                }
            }
        }
    }

    /// Seek to `t` seconds and display the frame containing `t` (frame-accurate:
    /// the last frame with pts <= t, decoded forward from the nearest keyframe).
    pub fn seek(&mut self, queue: &wgpu::Queue, t: f64) {
        let ts = (t.max(0.0) * AV_TIME_BASE) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        self.pending = None;

        let mut last: Option<(ffmpeg::frame::Video, f64)> = None;
        loop {
            match self.decode_next_raw() {
                Some((frame, pts)) => {
                    if pts > t {
                        let (to_upload, upload_pts) =
                            last.take().unwrap_or_else(|| (frame.clone(), pts));
                        self.scale_and_upload(queue, &to_upload);
                        self.displayed_pts = Some(upload_pts);
                        self.pending = Some((frame, pts));
                        return;
                    }
                    last = Some((frame, pts));
                }
                None => {
                    if let Some((prev, prev_pts)) = last.take() {
                        self.scale_and_upload(queue, &prev);
                        self.displayed_pts = Some(prev_pts);
                    }
                    return;
                }
            }
        }
    }

    fn decode_next_raw(&mut self) -> Option<(ffmpeg::frame::Video, f64)> {
        let mut frame = ffmpeg::frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let pts = frame.pts().unwrap_or(0);
                    let pts_sec = pts as f64 * self.time_base_seconds;
                    return Some((frame, pts_sec));
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

    fn scale_and_upload(&mut self, queue: &wgpu::Queue, frame: &ffmpeg::frame::Video) {
        if self.scaler.run(frame, &mut self.scaled).is_err() {
            return;
        }
        let stride = self.scaled.stride(0);
        let row_bytes = (self.width * 4) as usize;
        let src = self.scaled.data(0);
        for y in 0..self.height as usize {
            let src_off = y * stride;
            let dst_off = y * row_bytes;
            self.rgba_buf[dst_off..dst_off + row_bytes]
                .copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
        self.texture
            .write_region(queue, 0, 0, self.width, self.height, &self.rgba_buf);
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

fn bake_thumbnail(
    queue: &wgpu::Queue,
    scaler: &mut ffmpeg::software::scaling::Context,
    frame: &ffmpeg::frame::Video,
    w: u32,
    h: u32,
    texture: &Texture,
) {
    let mut scaled = ffmpeg::frame::Video::empty();
    if scaler.run(frame, &mut scaled).is_err() {
        return;
    }
    let stride = scaled.stride(0);
    let row_bytes = (w * 4) as usize;
    let src = scaled.data(0);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        let src_off = y * stride;
        let dst_off = y * row_bytes;
        buf[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
    }
    texture.write_region(queue, 0, 0, w, h, &buf);
}
