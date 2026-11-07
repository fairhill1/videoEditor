use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;

use crate::colorspace;
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
    /// The frames decoded most recently, so a step back can be answered
    /// without going back to a keyframe. See [`Recent`].
    recent: Recent<ffmpeg::frame::Video>,
    rgba_buf: Vec<u8>,
    scaled: ffmpeg::frame::Video,
    displayed_pts: Option<f64>,
    /// When the preview last asked for a frame, which is every frame while
    /// this stream is on screen. One that has gone quiet hands its cache back.
    last_used: Instant,
}

/// Short side of the baked media-pool thumbnail. The pool slot is a fixed
/// aspect ratio, so the UI letterboxes the thumb into its slot — we only need
/// enough pixels here to look crisp at typical row sizes.
const THUMB_HEIGHT: u32 = 64;

/// Most decoded-frame memory one stream keeps for stepping backwards: about
/// half a second of 1440p, a second of 1080p.
///
/// Stepping back is what this is for. A compressed frame can only be rebuilt
/// by decoding forward from the keyframe before it, and recordings put those
/// seconds apart, so without a cache every press of Left decodes a GOP — up to
/// a few hundred frames — to show one. With it, only every Nth press does.
const RECENT_BYTES: usize = 192 << 20;

/// However big a frame, the cache holds the one on screen and the one after it,
/// which is what playing forward needs.
const RECENT_MIN: usize = 2;

/// How long a stream can go without being shown before it gives its cache
/// back. Long enough to outlast a hitch in rendering, short enough that a pool
/// of clips that have each been on screen once doesn't sit on hundreds of
/// megabytes per clip.
const RECENT_IDLE: Duration = Duration::from_secs(1);

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

        let mut ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
        // libavcodec decodes on one thread unless asked otherwise, and one
        // core cannot keep up with 1440p60 HEVC: ~17ms a frame against a
        // 16.7ms budget, before conversion and upload. Frame threading on
        // every core brings that to ~3ms, at the cost of a few frames' extra
        // latency after a seek.
        ctx.set_threading(ffmpeg::codec::threading::Config {
            kind: ffmpeg::codec::threading::Type::Frame,
            count: 0,
        });
        let decoder = ctx.decoder().video()?;

        let width = decoder.width();
        let height = decoder.height();
        let matrix = colorspace::matrix_for(decoder.color_space(), height);

        let mut scaler = ffmpeg::software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;
        // Both scalers are built once per stream and never rebuilt, so the
        // matrix set here holds for every frame.
        if !colorspace::use_matrix(&mut scaler, matrix) {
            log::warn!("{path}: preview kept the default colour matrix");
        }

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
        colorspace::use_matrix(&mut thumb_scaler, matrix);
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
            recent: Recent::new(RECENT_MIN),
            rgba_buf: vec![0u8; (width * height * 4) as usize],
            scaled: ffmpeg::frame::Video::empty(),
            displayed_pts: None,
            last_used: Instant::now(),
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
            stream.recent.cap = recent_cap(&frame);
            stream.recent.push(frame, pts);
            stream.show(queue, 0);
        }

        Ok(stream)
    }

    /// Display the frame at `t` using whichever strategy is cheapest.
    /// - `t` falls among the frames decoded recently — a step back, or a
    ///   repeat of the same time while paused: show it from there.
    /// - `t` is close ahead: let the decoder walk forward (cheap).
    /// - anything else: seek.
    /// This is the only entry point callers should use during playback/scrub;
    /// it keeps per-frame decoder work bounded even under rapid seek requests.
    pub fn goto(&mut self, queue: &wgpu::Queue, t: f64) {
        self.last_used = Instant::now();
        if let Some(i) = self.recent.covering(t) {
            self.show(queue, i);
            return;
        }
        match self.recent.newest_pts() {
            Some(newest) if t >= newest && t <= newest + FORWARD_DECODE_BUDGET => {
                self.decode_through(queue, t)
            }
            _ => self.seek(queue, t),
        }
    }

    /// Give back the cached frames behind the one on screen if nothing has
    /// shown this stream for a while. The frame on screen and the one after it
    /// stay, so playing on from here still doesn't have to seek.
    pub fn release_idle(&mut self) {
        if self.last_used.elapsed() < RECENT_IDLE {
            return;
        }
        if let Some(shown) = self.displayed_pts {
            self.recent.drop_before(shown);
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

    /// Decode forward until the frame after `t` is in hand, then show the one
    /// at `t`. Every frame decoded on the way goes into the cache, so stepping
    /// back over them later costs nothing; only the one shown is converted and
    /// uploaded.
    fn decode_through(&mut self, queue: &wgpu::Queue, t: f64) {
        while self.recent.newest_pts().is_none_or(|newest| newest <= t) {
            match self.decode_next_raw() {
                Some((frame, pts)) => self.recent.push(frame, pts),
                // The end of the file: the last frame is as close as it gets.
                None => break,
            }
        }
        if let Some(i) = self.recent.at_or_before(t) {
            self.show(queue, i);
        }
    }

    /// Seek to `t` seconds and display the frame containing `t` (frame-accurate:
    /// the last frame with pts <= t, decoded forward from the keyframe before
    /// it).
    pub fn seek(&mut self, queue: &wgpu::Queue, t: f64) {
        let ts = (t.max(0.0) * AV_TIME_BASE) as i64;
        // The keyframe at or before `t`; failing that, the nearest one.
        if self.ictx.seek(ts, ..ts).is_err() {
            let _ = self.ictx.seek(ts, ..);
        }
        self.decoder.flush();
        self.recent.clear();
        self.decode_through(queue, t);
    }

    /// Convert and upload cached frame `i`, unless it is already on screen.
    fn show(&mut self, queue: &wgpu::Queue, i: usize) {
        let Self { recent, scaler, scaled, rgba_buf, texture, width, height, displayed_pts, .. } =
            self;
        let Some((frame, pts)) = recent.get(i) else {
            return;
        };
        if *displayed_pts == Some(pts) {
            return;
        }
        if scaler.run(frame, scaled).is_err() {
            return;
        }
        let stride = scaled.stride(0);
        let row_bytes = (*width * 4) as usize;
        let src = scaled.data(0);
        for y in 0..*height as usize {
            rgba_buf[y * row_bytes..(y + 1) * row_bytes]
                .copy_from_slice(&src[y * stride..y * stride + row_bytes]);
        }
        texture.write_region(queue, 0, 0, *width, *height, rgba_buf);
        *displayed_pts = Some(pts);
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

/// How many frames like `frame` fit in [`RECENT_BYTES`].
fn recent_cap(frame: &ffmpeg::frame::Video) -> usize {
    let bytes: usize = (0..frame.planes())
        .map(|i| frame.stride(i) * frame.plane_height(i) as usize)
        .sum();
    (RECENT_BYTES / bytes.max(1)).max(RECENT_MIN)
}

/// The last frames a decoder produced, oldest first, each with its time in
/// seconds. Contiguous: every frame decoded since the last seek goes in, so
/// the run has no holes, and a time between its first and last frame is
/// answered exactly. Bounded at `cap` frames by dropping the oldest.
///
/// Generic over the frame so the bookkeeping can be tested without a decoder.
struct Recent<F> {
    frames: VecDeque<(F, f64)>,
    cap: usize,
}

impl<F> Recent<F> {
    fn new(cap: usize) -> Self {
        Self { frames: VecDeque::new(), cap }
    }

    fn push(&mut self, frame: F, pts: f64) {
        self.frames.push_back((frame, pts));
        while self.frames.len() > self.cap.max(1) {
            self.frames.pop_front();
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
    }

    fn get(&self, i: usize) -> Option<(&F, f64)> {
        self.frames.get(i).map(|(f, pts)| (f, *pts))
    }

    fn newest_pts(&self) -> Option<f64> {
        self.frames.back().map(|(_, pts)| *pts)
    }

    /// The frame showing at `t`, if the run is known to contain it: `t` is at
    /// or after the oldest frame and before the newest. At or past the newest
    /// the answer may be a frame not decoded yet, so it is not an answer.
    fn covering(&self, t: f64) -> Option<usize> {
        let (_, oldest) = self.frames.front()?;
        let newest = self.newest_pts()?;
        if t < *oldest || t >= newest {
            return None;
        }
        self.at_or_before(t)
    }

    /// The last frame at or before `t`; the oldest one if every frame is after
    /// it, which is what a seek that landed past `t` has to settle for.
    fn at_or_before(&self, t: f64) -> Option<usize> {
        if self.frames.is_empty() {
            return None;
        }
        let after = self.frames.partition_point(|(_, pts)| *pts <= t);
        Some(after.saturating_sub(1))
    }

    /// Forget every frame before `pts`.
    fn drop_before(&mut self, pts: f64) {
        while self.frames.front().is_some_and(|(_, p)| *p < pts) {
            self.frames.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames at 0.0, 0.1, ... 0.9.
    fn run(cap: usize) -> Recent<()> {
        let mut r = Recent::new(cap);
        for i in 0..10 {
            r.push((), i as f64 / 10.0);
        }
        r
    }

    #[test]
    fn a_step_back_inside_the_run_is_answered_from_it() {
        let r = run(100);
        assert_eq!(r.covering(0.55), Some(5));
        assert_eq!(r.covering(0.5), Some(5));
        assert_eq!(r.covering(0.0), Some(0));
    }

    #[test]
    fn a_time_at_or_past_the_newest_frame_goes_to_the_decoder() {
        let r = run(100);
        // The frame after 0.9 might be due at 0.95; only decoding can say.
        assert_eq!(r.covering(0.9), None);
        assert_eq!(r.covering(1.5), None);
    }

    #[test]
    fn a_time_before_the_run_goes_to_the_decoder() {
        let r = run(4);
        // Only 0.6..=0.9 are left.
        assert_eq!(r.covering(0.5), None);
        assert_eq!(r.covering(0.65), Some(0));
    }

    #[test]
    fn the_run_is_bounded_by_dropping_the_oldest() {
        let r = run(3);
        assert_eq!(r.frames.len(), 3);
        assert_eq!(r.get(0).map(|(_, p)| p), Some(0.7));
    }

    #[test]
    fn a_seek_that_overshoots_settles_for_the_first_frame() {
        let r = run(100);
        assert_eq!(r.at_or_before(-1.0), Some(0));
        assert_eq!(r.at_or_before(5.0), Some(9));
    }

    #[test]
    fn an_idle_stream_keeps_the_frame_on_screen_and_what_follows() {
        let mut r = run(100);
        r.drop_before(0.8);
        assert_eq!(r.frames.len(), 2);
        assert_eq!(r.covering(0.85), Some(0));
    }
}
