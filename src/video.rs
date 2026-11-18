use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;

use crate::colorspace;
use crate::quad::{QuadRenderer, Texture};

const AV_TIME_BASE: f64 = 1_000_000.0;

pub struct VideoStream {
    /// Kept so a standby decoder can open the file again on its own thread.
    path: String,
    decoder: Decoder,
    /// A second decoder for the same file, sent ahead to where the next cut
    /// lands. See [`VideoStream::preroll`].
    standby: Option<Standby>,
    scaler: ffmpeg::software::scaling::Context,
    duration_seconds: f64,
    width: u32,
    height: u32,
    frame_rate: f64,
    texture: Texture,
    thumbnail: Texture,
    rgba_buf: Vec<u8>,
    scaled: ffmpeg::frame::Video,
    displayed_pts: Option<f64>,
    /// When the preview last asked for a frame, which is every frame while
    /// this stream is on screen. One that has gone quiet hands its cache back.
    last_used: Instant,
}

/// One place in a file: the demuxer, the codec, and the frames it decoded
/// most recently. Everything here is safe to build on another thread and hand
/// over, which is what a standby is.
struct Decoder {
    ictx: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    time_base_seconds: f64,
    /// Told the decoder the file has run out. It holds frames back to put
    /// them in order, and one a thread when decoding on several, and gives
    /// them up only once it has been told: without that the last of the
    /// file never comes out and the picture freezes short of the end.
    drained: bool,
    /// The frames decoded most recently, so a step back can be answered
    /// without going back to a keyframe. See [`Recent`].
    recent: Recent<ffmpeg::frame::Video>,
}

/// A decoder being readied for a cut, or ready for it.
struct Standby {
    /// What it was sent to: a frame before the cut's in-point, so the in-point
    /// itself is answered whichever side of a frame boundary it falls.
    target: f64,
    /// When a look-ahead last wanted it. One nobody has asked for in a while
    /// is for a cut playback never reached.
    asked: Instant,
    /// Where its thread hands it over once it has arrived, or `None` if the
    /// file would not open there.
    ready: Receiver<Option<Decoder>>,
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

/// What a standby keeps while it waits: the frames either side of its target
/// and little more, since all it has to do is take over at one instant.
const STANDBY_FRAMES: usize = 4;

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

/// How far ahead of the playhead the preview looks for cuts, in timeline
/// seconds. A standby has this long to seek and decode up to its in-point —
/// a whole GOP of a recording, which at 1440p can take most of a second on a
/// machine that is also playing the clip before it.
pub const PREROLL_AHEAD: f64 = 2.0;

/// How many times across [`PREROLL_AHEAD`] the look-ahead samples the
/// timeline. A clip shorter than the spacing can go unseen.
pub const PREROLL_SAMPLES: usize = 8;

impl VideoStream {
    pub fn open(
        path: &str,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        quads: &QuadRenderer,
    ) -> Result<Self, ffmpeg::Error> {
        let (mut decoder, frame_rate) = Decoder::open(path)?;

        let duration_raw = decoder.ictx.duration();
        let duration_seconds = if duration_raw > 0 {
            duration_raw as f64 / AV_TIME_BASE
        } else {
            0.0
        };

        let width = decoder.decoder.width();
        let height = decoder.decoder.height();
        let format = decoder.decoder.format();
        let matrix = colorspace::matrix_for(decoder.decoder.color_space(), height);

        let mut scaler = ffmpeg::software::scaling::Context::get(
            format,
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
            format,
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

        // Prime with the first frame so nothing flashes black at startup.
        // Bake the pool thumbnail from the same frame — it's already decoded
        // and we only need it once, so reusing it avoids a second seek/decode.
        let first = decoder.decode_next_raw();
        if let Some((frame, pts)) = first {
            bake_thumbnail(queue, &mut thumb_scaler, &frame, thumb_w, thumb_h, &thumbnail);
            decoder.recent.cap = recent_cap(&frame);
            decoder.recent.push(frame, pts);
        }

        let mut stream = Self {
            path: path.to_string(),
            decoder,
            standby: None,
            scaler,
            duration_seconds,
            width,
            height,
            frame_rate,
            texture,
            thumbnail,
            rgba_buf: vec![0u8; (width * height * 4) as usize],
            scaled: ffmpeg::frame::Video::empty(),
            displayed_pts: None,
            last_used: Instant::now(),
        };
        stream.show(queue, 0);

        Ok(stream)
    }

    /// Display the frame at `t` using whichever strategy is cheapest.
    /// - `t` falls among the frames decoded recently — a step back, or a
    ///   repeat of the same time while paused: show it from there.
    /// - `t` is close ahead: let the decoder walk forward (cheap).
    /// - a standby was sent to `t` ahead of a cut: take it over.
    /// - anything else: seek.
    /// This is the only entry point callers should use during playback/scrub;
    /// it keeps per-frame decoder work bounded even under rapid seek requests.
    pub fn goto(&mut self, queue: &wgpu::Queue, t: f64) {
        self.last_used = Instant::now();
        if !self.decoder.reaches(t) {
            self.take_standby(t);
        }
        if let Some(i) = self.decoder.recent.covering(t) {
            self.show(queue, i);
        } else if self.decoder.reaches(t) {
            self.decode_through(queue, t);
        } else {
            self.seek(queue, t);
        }
    }

    /// Get a decoder ready, off the render thread, for a clip of this file
    /// that comes on screen at `entry` in the source. `before` is where this
    /// file is showing just before that, if it is.
    ///
    /// Without this, a cut to a clip whose in-point is anywhere but just ahead
    /// of the decoder is a seek on the render thread at the moment of the cut:
    /// back to the keyframe and forward through a GOP, which for a 1440p
    /// recording is a visible stall. The standby does that work in the
    /// seconds before, and [`Self::goto`] swaps it in when the cut arrives.
    ///
    /// Called every frame while playing, for every cut in the look-ahead; a
    /// standby already sent to the same place is left to carry on.
    pub fn preroll(&mut self, entry: f64, before: Option<f64>) {
        let reachable = match before {
            // On screen up to the cut: it will be where the outgoing clip
            // ends, and can walk on from there if the in-point is just ahead.
            Some(at) => entry >= at && entry <= at + FORWARD_DECODE_BUDGET,
            // Off screen until the cut, so it will still be where it is now —
            // unless it was on screen a moment ago and is still moving on.
            None => {
                self.last_used.elapsed() >= RECENT_IDLE && self.decoder.reaches(entry)
            }
        };
        if reachable {
            return;
        }
        let target = (entry - 1.0 / self.frame_rate).max(0.0);
        if let Some(standby) = self.standby.as_mut() {
            if (standby.target - target).abs() < 1e-3 {
                standby.asked = Instant::now();
                return;
            }
        }
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        let spawned = thread::Builder::new().name("video-preroll".into()).spawn(move || {
            let decoder = Decoder::open(&path).ok().map(|(mut d, _)| {
                d.recent.cap = STANDBY_FRAMES;
                d.seek_through(target);
                d
            });
            let _ = tx.send(decoder);
        });
        if spawned.is_err() {
            return;
        }
        if let Some(old) = self.standby.take() {
            drop_elsewhere(old);
        }
        self.standby = Some(Standby {
            target,
            asked: Instant::now(),
            ready: rx,
        });
    }

    /// Give back the cached frames behind the one on screen if nothing has
    /// shown this stream for a while. The frame on screen and the one after it
    /// stay, so playing on from here still doesn't have to seek. A standby no
    /// look-ahead has wanted for as long goes too.
    pub fn release_idle(&mut self) {
        if self.standby.as_ref().is_some_and(|s| s.asked.elapsed() >= RECENT_IDLE) {
            if let Some(old) = self.standby.take() {
                drop_elsewhere(old);
            }
        }
        if self.last_used.elapsed() < RECENT_IDLE {
            return;
        }
        if let Some(shown) = self.displayed_pts {
            self.decoder.recent.drop_before(shown);
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

    /// Make the standby the decoder, if it was sent to `t`. One still seeking
    /// is waited for: it is partway through the seek this would otherwise
    /// start from scratch.
    fn take_standby(&mut self, t: f64) {
        let Some(standby) = self.standby.take() else {
            return;
        };
        if t < standby.target || t > standby.target + FORWARD_DECODE_BUDGET {
            self.standby = Some(standby);
            return;
        }
        let Some(mut decoder) = standby.ready.recv().ok().flatten() else {
            return;
        };
        if !decoder.reaches(t) {
            drop_elsewhere(decoder);
            return;
        }
        decoder.recent.cap = self.decoder.recent.cap;
        let old = std::mem::replace(&mut self.decoder, decoder);
        drop_elsewhere(old);
    }

    /// Decode forward until the frame after `t` is in hand, then show the one
    /// at `t`. Every frame decoded on the way goes into the cache, so stepping
    /// back over them later costs nothing; only the one shown is converted and
    /// uploaded.
    fn decode_through(&mut self, queue: &wgpu::Queue, t: f64) {
        self.decoder.decode_through(t);
        if let Some(i) = self.decoder.recent.at_or_before(t) {
            self.show(queue, i);
        }
    }

    /// Seek to `t` seconds and display the frame containing `t` (frame-accurate:
    /// the last frame with pts <= t, decoded forward from the keyframe before
    /// it).
    pub fn seek(&mut self, queue: &wgpu::Queue, t: f64) {
        self.decoder.seek_through(t);
        if let Some(i) = self.decoder.recent.at_or_before(t) {
            self.show(queue, i);
        }
    }

    /// Convert and upload cached frame `i`, unless it is already on screen.
    fn show(&mut self, queue: &wgpu::Queue, i: usize) {
        let Self { decoder, scaler, scaled, rgba_buf, texture, width, height, displayed_pts, .. } =
            self;
        let Some((frame, pts)) = decoder.recent.get(i) else {
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
}

impl Decoder {
    /// Open the best video stream in `path`, with its frame rate.
    fn open(path: &str) -> Result<(Self, f64), ffmpeg::Error> {
        ffmpeg::init()?;

        let ictx = ffmpeg::format::input(&path)?;

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

        Ok((
            Self {
                ictx,
                decoder,
                stream_index,
                time_base_seconds,
                drained: false,
                recent: Recent::new(RECENT_MIN),
            },
            frame_rate,
        ))
    }

    /// Whether `t` can be shown without a seek: it is among the recent frames,
    /// or close enough ahead to decode forward to.
    fn reaches(&self, t: f64) -> bool {
        self.recent.covering(t).is_some()
            || self
                .recent
                .newest_pts()
                .is_some_and(|newest| t >= newest && t <= newest + FORWARD_DECODE_BUDGET)
    }

    /// Decode forward until the frame after `t` is in hand.
    fn decode_through(&mut self, t: f64) {
        while self.recent.newest_pts().is_none_or(|newest| newest <= t) {
            match self.decode_next_raw() {
                Some((frame, pts)) => self.recent.push(frame, pts),
                // The end of the file: the last frame is as close as it gets.
                None => break,
            }
        }
    }

    /// Seek to the keyframe at or before `t` and decode forward through it.
    fn seek_through(&mut self, t: f64) {
        let ts = (t.max(0.0) * AV_TIME_BASE) as i64;
        // The keyframe at or before `t`; failing that, the nearest one.
        if self.ictx.seek(ts, ..ts).is_err() {
            let _ = self.ictx.seek(ts, ..);
        }
        self.decoder.flush();
        self.drained = false;
        self.recent.clear();
        self.decode_through(t);
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
                Err(_) if self.drained => return None,
                Err(_) => match self.next_video_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => {
                        let _ = self.decoder.send_eof();
                        self.drained = true;
                    }
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

/// Drop a decoder, or anything holding one, on a thread of its own. Closing
/// a frame-threaded codec joins its worker threads, which is not a wait the
/// render thread should take in the middle of a cut.
fn drop_elsewhere<T: Send + 'static>(value: T) {
    let _ = thread::Builder::new().name("video-drop".into()).spawn(move || drop(value));
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

    /// Decoding to the end of a file gives up every frame in it, the ones
    /// the decoder holds back included: the last frame out is the file's
    /// last, not one a few frames short of it that the picture freezes on.
    #[test]
    fn decoding_to_the_end_reaches_the_last_frame() {
        let Ok(list) = std::env::var("RUVE_TEST_MEDIA") else {
            eprintln!("RUVE_TEST_MEDIA unset; skipping");
            return;
        };
        for path in list.split(',') {
            let (mut d, _) = Decoder::open(path).unwrap();
            // The last frame in the file, by its packets: those marked to be
            // decoded and thrown away, as a recording can end on, show nothing.
            let mut ictx = ffmpeg::format::input(&path).unwrap();
            let last = ictx
                .packets()
                .filter(|(s, _)| s.index() == d.stream_index)
                .filter(|(_, p)| {
                    use ffmpeg::packet::Ref;
                    // SAFETY: reads a field of a packet held for the call.
                    unsafe { (*p.as_ptr()).flags & ffmpeg::ffi::AV_PKT_FLAG_DISCARD == 0 }
                })
                .filter_map(|(_, p)| p.pts())
                .max()
                .unwrap() as f64
                * d.time_base_seconds;
            d.seek_through((last - 1.0).max(0.0));
            d.decode_through(last + 10.0);
            let newest = d.recent.newest_pts().unwrap();
            assert!((newest - last).abs() < 1e-6, "{path}: ended at {newest:.4}s, the file at {last:.4}s");
        }
    }

    /// A standby readied on another thread, the way a look-ahead sends one,
    /// arrives able to show the in-point without a seek, and shows the frame
    /// a seek straight to it would have.
    #[test]
    fn a_standby_arrives_at_its_in_point() {
        let Ok(list) = std::env::var("RUVE_TEST_MEDIA") else {
            eprintln!("RUVE_TEST_MEDIA unset; skipping");
            return;
        };
        let path = list.split(',').next().unwrap().to_string();
        let (mut direct, fps) = Decoder::open(&path).unwrap();
        let entry = (direct.ictx.duration() as f64 / AV_TIME_BASE * 0.5).max(0.0);
        let started = Instant::now();
        direct.seek_through(entry);
        eprintln!("seeking to {entry:.3}s on the spot took {:?}", started.elapsed());
        let want = direct.recent.get(direct.recent.at_or_before(entry).unwrap()).unwrap().1;

        let target = (entry - 1.0 / fps).max(0.0);
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        thread::spawn(move || {
            let (mut d, _) = Decoder::open(&p).unwrap();
            d.recent.cap = STANDBY_FRAMES;
            d.seek_through(target);
            tx.send(d).unwrap();
        });
        let mut standby = rx.recv().unwrap();
        assert!(standby.reaches(entry));
        let started = Instant::now();
        standby.decode_through(entry);
        let got = standby.recent.get(standby.recent.at_or_before(entry).unwrap()).unwrap().1;
        eprintln!("taking over at {entry:.3}s decoded in {:?}", started.elapsed());
        assert_eq!(got, want);
    }

    #[test]
    fn an_idle_stream_keeps_the_frame_on_screen_and_what_follows() {
        let mut r = run(100);
        r.drop_before(0.8);
        assert_eq!(r.frames.len(), 2);
        assert_eq!(r.covering(0.85), Some(0));
    }
}
