//! Frames spread across a clip, for scanning its contents by hovering its
//! thumbnail in the media pool.
//!
//! Decoded on a thread of its own with its own demuxer and decoder, because a
//! strip of a long recording takes a second or so to build, and an import that
//! froze the window for that long would feel broken. The pool shows the poster
//! frame until the strip arrives, then scrubs through it.
//!
//! Each sample is the first frame within half a sample's spacing of its time.
//! Taking the nearest keyframe instead would be far cheaper, but a short clip
//! can have a single keyframe, which would fill every sample with the poster.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use ffmpeg_next as ffmpeg;

use crate::colorspace;
use crate::quad::{QuadRenderer, Texture};

/// Samples per clip. Enough that sweeping across a thumbnail reads as motion
/// rather than as a slideshow, few enough to build in about a second.
pub(crate) const FRAMES: u32 = 30;
/// Laid out in this many columns in the atlas, keeping it well inside the
/// texture size limit for any sane aspect ratio.
const COLS: u32 = 6;
/// Height of one sample. Sharp at the size of a grid tile on a HiDPI screen.
const FRAME_H: u32 = 180;

const AV_TIME_BASE: f64 = 1_000_000.0;
/// How far ahead the next sample must be before the decoder seeks to it rather
/// than decoding forward. About a GOP of a typical recording: any nearer and
/// the seek would land on the keyframe already behind it, and decode the same
/// frames again.
const SEEK_AHEAD: f64 = 2.0;

/// The pixels the decode thread hands back: every sample, packed into one
/// RGBA atlas `COLS` frames wide.
struct Strip {
    frame_w: u32,
    frame_h: u32,
    rgba: Vec<u8>,
}

/// A clip's strip: still being decoded, ready to draw, or given up on.
pub(crate) struct Filmstrip {
    pending: Option<Receiver<Strip>>,
    atlas: Option<Texture>,
    frame_w: u32,
    frame_h: u32,
}

impl Filmstrip {
    /// Start building the strip for the video in `path`, `duration` seconds
    /// long. Returns at once; [`Filmstrip::poll`] picks the result up.
    pub(crate) fn spawn(path: &str, duration: f64) -> Self {
        let (tx, rx) = mpsc::channel();
        let path = path.to_string();
        let spawned = std::thread::Builder::new()
            .name("filmstrip".into())
            .spawn(move || match decode(&path, duration) {
                // A send that fails means the source was dropped while this
                // ran, and nobody is left to want the strip.
                Ok(strip) => drop(tx.send(strip)),
                Err(e) => log::warn!("{path}: no hover filmstrip: {e}"),
            });
        if let Err(e) = spawned {
            log::warn!("could not start the filmstrip thread: {e}");
        }
        Self { pending: Some(rx), atlas: None, frame_w: 0, frame_h: 0 }
    }

    /// Upload the strip if the thread has finished it. Cheap when it hasn't,
    /// so the render loop calls it every frame.
    pub(crate) fn poll(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, quads: &QuadRenderer) {
        let Some(rx) = self.pending.as_ref() else {
            return;
        };
        let strip = match rx.try_recv() {
            Ok(strip) => strip,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                // The thread failed and has said why in the log; the pool
                // keeps showing the poster frame.
                self.pending = None;
                return;
            }
        };
        self.pending = None;
        let (w, h) = atlas_size(strip.frame_w, strip.frame_h);
        let texture =
            quads.create_empty_texture(device, w, h, wgpu::TextureFormat::Rgba8UnormSrgb);
        texture.write_region(queue, 0, 0, w, h, &strip.rgba);
        self.atlas = Some(texture);
        self.frame_w = strip.frame_w;
        self.frame_h = strip.frame_h;
    }

    /// The sample for a hover `along` the thumbnail (0 at its left edge, 1 at
    /// its right): the atlas and the uv rect of that sample in it. `None`
    /// until the strip has arrived.
    pub(crate) fn frame_at(&self, along: f32) -> Option<(&Texture, [f32; 4])> {
        let atlas = self.atlas.as_ref()?;
        let i = sample_index(along);
        let (col, row) = (i % COLS, i / COLS);
        let (aw, ah) = (atlas.width as f32, atlas.height as f32);
        let (fw, fh) = (self.frame_w as f32, self.frame_h as f32);
        let (u0, v0) = (col as f32 * fw / aw, row as f32 * fh / ah);
        Some((atlas, [u0, v0, u0 + fw / aw, v0 + fh / ah]))
    }
}

/// Which sample a hover `along` the thumbnail lands on. Evenly split, so each
/// sample owns the same width of thumbnail, and clamped so the right edge
/// itself lands on the last one.
fn sample_index(along: f32) -> u32 {
    ((along.clamp(0.0, 1.0) * FRAMES as f32) as u32).min(FRAMES - 1)
}

/// When sample `i` of a `duration`-second clip is taken. The first is the very
/// start, so a hover at the left edge shows what the poster frame shows.
fn sample_time(i: u32, duration: f64) -> f64 {
    duration * i as f64 / FRAMES as f64
}

fn atlas_size(frame_w: u32, frame_h: u32) -> (u32, u32) {
    (frame_w * COLS, frame_h * FRAMES.div_ceil(COLS))
}

/// Decode every sample of `path` into one atlas.
fn decode(path: &str, duration: f64) -> Result<Strip, ffmpeg::Error> {
    let mut ictx = ffmpeg::format::input(&path)?;
    let (stream_index, time_base, parameters) = {
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
    let mut ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
    // Runs of frames get decoded between samples, which is what frame
    // threading speeds up; see `VideoStream::open`.
    ctx.set_threading(ffmpeg::codec::threading::Config {
        kind: ffmpeg::codec::threading::Type::Frame,
        count: 0,
    });
    let mut decoder = ctx.decoder().video()?;
    let (width, height) = (decoder.width(), decoder.height());
    let frame_h = FRAME_H.min(height).max(1);
    let frame_w = ((frame_h as f64 * width as f64 / height.max(1) as f64).round() as u32).max(1);
    let mut scaler = ffmpeg::software::scaling::Context::get(
        decoder.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        frame_w,
        frame_h,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;
    colorspace::use_matrix(&mut scaler, colorspace::matrix_for(decoder.color_space(), height));

    let (atlas_w, atlas_h) = atlas_size(frame_w, frame_h);
    let mut rgba = vec![0u8; (atlas_w * atlas_h * 4) as usize];
    let mut scaled = ffmpeg::frame::Video::empty();
    let tolerance = duration / FRAMES as f64 / 2.0;
    // The last frame decoded and its time, whichever sample it went to.
    let mut last: Option<(ffmpeg::frame::Video, f64)> = None;
    for i in 0..FRAMES {
        let t = sample_time(i, duration);
        let want = t - tolerance;
        let at = last.as_ref().map(|(_, pts)| *pts);
        if at.is_none_or(|at| want - at > SEEK_AHEAD) {
            // The last keyframe before the far end of the sample's window, so
            // a keyframe inside the window is taken as it is, with nothing to
            // decode forward; failing that (a time before the first
            // keyframe), whichever is nearest.
            let ts = ((t + tolerance) * AV_TIME_BASE) as i64;
            if ictx.seek(ts, ..ts).is_ok() || ictx.seek(ts, ..).is_ok() {
                decoder.flush();
            }
        }
        // Forward to the sample, unless the frame in hand is already there:
        // samples closer together than frames are, on a very short clip.
        while last.as_ref().is_none_or(|(_, pts)| *pts < want) {
            match next_frame(&mut ictx, &mut decoder, stream_index, time_base) {
                Some(next) => last = Some(next),
                // The end of the file: the last frame is the nearest there is.
                None => break,
            }
        }
        let Some((frame, _)) = last.as_ref() else {
            continue;
        };
        if scaler.run(frame, &mut scaled).is_err() {
            continue;
        }
        let (col, row) = (i % COLS, i / COLS);
        let stride = scaled.stride(0);
        let row_bytes = (frame_w * 4) as usize;
        let src = scaled.data(0);
        for y in 0..frame_h as usize {
            let dst_y = row as usize * frame_h as usize + y;
            let dst = (dst_y * atlas_w as usize + col as usize * frame_w as usize) * 4;
            rgba[dst..dst + row_bytes].copy_from_slice(&src[y * stride..y * stride + row_bytes]);
        }
    }
    Ok(Strip { frame_w, frame_h, rgba })
}

/// The next picture out of the decoder and its time in seconds, feeding it
/// packets until one comes. `None` at the end of the file.
fn next_frame(
    ictx: &mut ffmpeg::format::context::Input,
    decoder: &mut ffmpeg::decoder::Video,
    stream_index: usize,
    time_base: f64,
) -> Option<(ffmpeg::frame::Video, f64)> {
    let mut frame = ffmpeg::frame::Video::empty();
    let mut packets = ictx.packets();
    loop {
        if decoder.receive_frame(&mut frame).is_ok() {
            let pts = frame.timestamp().or(frame.pts()).unwrap_or(0) as f64 * time_base;
            return Some((frame, pts));
        }
        match packets.next() {
            Some((stream, packet)) => {
                if stream.index() == stream_index {
                    let _ = decoder.send_packet(&packet);
                }
            }
            None => {
                // Drain what the decoder still holds; frame threading keeps
                // several pictures in flight right up to the last packet.
                let _ = decoder.send_eof();
                return decoder.receive_frame(&mut frame).is_ok().then(|| {
                    let pts = frame.timestamp().or(frame.pts()).unwrap_or(0) as f64 * time_base;
                    (frame, pts)
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_width_of_a_thumbnail_maps_onto_the_samples() {
        assert_eq!(sample_index(0.0), 0);
        assert_eq!(sample_index(1.0), FRAMES - 1);
        assert_eq!(sample_index(0.999), FRAMES - 1);
        // Past either edge, as a hover on the border can be.
        assert_eq!(sample_index(-0.2), 0);
        assert_eq!(sample_index(1.3), FRAMES - 1);
    }

    #[test]
    fn samples_start_at_the_poster_frame_and_stop_short_of_the_end() {
        assert_eq!(sample_time(0, 60.0), 0.0);
        let last = sample_time(FRAMES - 1, 60.0);
        assert!(last < 60.0 && last > 55.0, "{last}");
    }

    /// Decodes the first file in `RUVE_TEST_MEDIA` and checks the strip holds
    /// pictures, and not all the same one: sampling only keyframes did that to
    /// a short clip with a single keyframe. Skips without the variable.
    #[test]
    fn a_real_clip_gives_a_strip_of_distinct_pictures() {
        let Ok(list) = std::env::var("RUVE_TEST_MEDIA") else {
            return;
        };
        let path = list.split(',').next().unwrap().trim();
        let duration = ffmpeg::format::input(&path).unwrap().duration() as f64 / AV_TIME_BASE;
        let started = std::time::Instant::now();
        let strip = decode(path, duration).unwrap();
        eprintln!("{path}: {FRAMES} samples in {:?}", started.elapsed());

        let (aw, _) = atlas_size(strip.frame_w, strip.frame_h);
        let cell = |i: u32| -> Vec<u8> {
            let (col, row) = (i % COLS, i / COLS);
            let mut out = Vec::new();
            for y in 0..strip.frame_h {
                let at = (((row * strip.frame_h + y) * aw + col * strip.frame_w) * 4) as usize;
                out.extend_from_slice(&strip.rgba[at..at + (strip.frame_w * 4) as usize]);
            }
            out
        };
        let cells: Vec<Vec<u8>> = (0..FRAMES).map(cell).collect();
        let lit = cells.iter().filter(|c| c.iter().any(|&b| b > 16)).count();
        assert_eq!(lit, FRAMES as usize, "some samples came out blank");
        let mut distinct = cells.clone();
        distinct.sort();
        distinct.dedup();
        // Only more than one: a screen recording can sit still for most of its
        // length, and that is the footage, not the strip.
        assert!(distinct.len() > 1, "every sample is the same picture");
    }

    #[test]
    fn every_sample_has_a_cell_in_the_atlas() {
        let (w, h) = atlas_size(320, 180);
        assert_eq!(w, 320 * COLS);
        assert!(h / 180 * COLS >= FRAMES);
    }
}
