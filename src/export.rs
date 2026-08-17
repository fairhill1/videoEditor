//! Offline render of the timeline to an H.264/AAC file.
//!
//! Deliberately independent of the preview pipeline: the worker opens its own
//! decoders straight from the source paths instead of borrowing the ones behind
//! the media pool. That means an export can run on a background thread while
//! you keep editing, and neither side disturbs the other's seek position.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ffmpeg;
use ffmpeg::software::scaling;
use ffmpeg::{codec, encoder, format, frame, picture, ChannelLayout, Dictionary, Packet, Rational};

use crate::audio::{AudioStream, CHANNELS, SAMPLE_RATE};
use crate::timeline::{Clip, SourceId, TrackKind};

/// x264 knobs. `medium` is the usual speed/size compromise, and crf 20 is
/// visually near-transparent on edited footage without producing huge files.
const X264_OPTS: [(&str, &str); 2] = [("preset", "medium"), ("crf", "20")];
const AUDIO_BIT_RATE: usize = 192_000;
/// Keyframe interval, in seconds. Frequent enough that the result scrubs
/// responsively in a player.
const GOP_SECONDS: f64 = 2.0;
/// How far ahead a source decoder walks before giving up and seeking. Same
/// reasoning as the preview decoder: keeps per-frame work bounded when the
/// timeline jumps around inside a source.
const FORWARD_DECODE_BUDGET: f64 = 1.0;
/// Limited-range black in YUV420P, matching the range x264 signals by default,
/// so letterbox bars sit at the same level as genuinely black picture.
const BLACK_Y: u8 = 16;
const BLACK_UV: u8 = 128;
/// Re-seek an audio source once its read head drifts this far from where the
/// mix wants to sample next. Mirrors the live mixer's tolerance.
const RESEEK_THRESHOLD_SEC: f64 = 0.030;
/// Used when a codec reports no fixed frame size. AAC always reports 1024, so
/// this is only a guard against a surprising encoder.
const FALLBACK_FRAME_SIZE: usize = 1024;

/// Geometry and cadence of the rendered picture, decided by the caller from the
/// timeline's reference clip.
#[derive(Clone, Copy)]
pub struct VideoSpec {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

pub struct ExportRequest {
    pub output: PathBuf,
    /// `None` when the timeline holds no video, which renders audio-only.
    pub video: Option<VideoSpec>,
    pub tracks: Vec<(TrackKind, Vec<Clip>)>,
    /// Source id to file path. Resolved by the caller so the worker never
    /// touches the media pool.
    pub paths: HashMap<SourceId, String>,
}

pub enum Outcome {
    Done(PathBuf),
    Cancelled,
    Failed(String),
}

#[derive(Clone, Copy, Default)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).clamp(0.0, 1.0)
        }
    }
}

struct Shared {
    progress: Progress,
    outcome: Option<Outcome>,
}

/// Handle on a running export. Dropping it does not stop the worker — the
/// thread is detached, so a job outlives its handle and finishes writing.
pub struct ExportJob {
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
}

impl ExportJob {
    pub fn start(req: ExportRequest) -> ExportJob {
        let shared = Arc::new(Mutex::new(Shared {
            progress: Progress::default(),
            outcome: None,
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let job = ExportJob {
            shared: shared.clone(),
            cancel: cancel.clone(),
        };
        std::thread::spawn(move || {
            let outcome = match run(&req, &shared, &cancel) {
                Ok(true) => Outcome::Done(req.output.clone()),
                // A half-written file has no moov atom and won't play, so it is
                // worse than no file at all. The muxer already truncated
                // whatever was at this path when it opened it, so there is no
                // pre-existing content left to preserve either way.
                Ok(false) => {
                    discard_partial(&req.output);
                    Outcome::Cancelled
                }
                Err(e) => {
                    discard_partial(&req.output);
                    Outcome::Failed(e)
                }
            };
            shared.lock().unwrap().outcome = Some(outcome);
        });
        job
    }

    pub fn progress(&self) -> Progress {
        self.shared.lock().unwrap().progress
    }

    /// Takes the result once the worker has finished, leaving `None` behind.
    /// Callers poll this and drop the job when it yields something.
    pub fn take_outcome(&self) -> Option<Outcome> {
        self.shared.lock().unwrap().outcome.take()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn discard_partial(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        log::warn!("could not remove partial export {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------------------

/// Returns `Ok(true)` on a completed render and `Ok(false)` if cancelled.
fn run(req: &ExportRequest, shared: &Mutex<Shared>, cancel: &AtomicBool) -> Result<bool, String> {
    ffmpeg::init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

    let duration = req
        .tracks
        .iter()
        .flat_map(|(_, clips)| clips.iter().map(|c| c.timeline_end()))
        .fold(0.0_f64, f64::max);

    let has_video = req
        .tracks
        .iter()
        .any(|(kind, clips)| *kind == TrackKind::Video && !clips.is_empty());
    let has_audio = req
        .tracks
        .iter()
        .any(|(kind, clips)| *kind == TrackKind::Audio && !clips.is_empty());
    if duration <= 0.0 || (!has_video && !has_audio) {
        return Err("nothing on the timeline to export".into());
    }
    let spec = req.video.filter(|_| has_video);

    let fps = spec.map_or(0.0, |s| s.fps);
    let total_frames = spec.map_or(0, |_| (duration * fps).ceil() as u64);
    let total_samples = if has_audio {
        (duration * SAMPLE_RATE as f64).ceil() as u64
    } else {
        0
    };
    // Progress counts output video frames; an audio-only render counts samples
    // instead so the bar still moves.
    shared.lock().unwrap().progress = Progress {
        done: 0,
        total: if total_frames > 0 {
            total_frames
        } else {
            total_samples
        },
    };

    let mut octx = format::output(&req.output)
        .map_err(|e| format!("cannot write {}: {e}", req.output.display()))?;
    let global_header = octx
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);

    let mut video = match spec {
        Some(spec) => {
            let (enc, enc_tb) = open_video_encoder(spec, global_header)
                .map_err(|e| format!("H.264 encoder setup failed: {e}"))?;
            let stream_idx = {
                let mut stream = octx
                    .add_stream(encoder::find(codec::Id::H264))
                    .map_err(|e| format!("could not add a video stream: {e}"))?;
                stream.set_parameters(&enc);
                stream.set_time_base(enc_tb);
                stream.index()
            };
            let takes = open_video_takes(req, spec)?;
            Some(VideoRender {
                enc,
                enc_tb,
                stream_idx,
                stream_tb: enc_tb,
                spec,
                takes,
            })
        }
        None => None,
    };

    let mut audio = if has_audio {
        let enc = open_audio_encoder(global_header)
            .map_err(|e| format!("AAC encoder setup failed: {e}"))?;
        let enc_tb = Rational(1, SAMPLE_RATE as i32);
        let stream_idx = {
            let mut stream = octx
                .add_stream(encoder::find(codec::Id::AAC))
                .map_err(|e| format!("could not add an audio stream: {e}"))?;
            stream.set_parameters(&enc);
            stream.set_time_base(enc_tb);
            stream.index()
        };
        let block = match enc.frame_size() as usize {
            0 => FALLBACK_FRAME_SIZE,
            n => n,
        };
        Some(AudioRender {
            enc,
            enc_tb,
            stream_idx,
            stream_tb: enc_tb,
            takes: open_audio_takes(req)?,
            block,
            mixed: vec![0.0; block * CHANNELS],
            scratch: vec![0.0; block * CHANNELS],
            samples_done: 0,
        })
    } else {
        None
    };

    octx.write_header()
        .map_err(|e| format!("could not write the file header: {e}"))?;

    // The muxer is free to rewrite stream time bases while writing the header,
    // so read them back before rescaling anything into them.
    if let Some(v) = video.as_mut() {
        v.stream_tb = octx.stream(v.stream_idx).unwrap().time_base();
    }
    if let Some(a) = audio.as_mut() {
        a.stream_tb = octx.stream(a.stream_idx).unwrap().time_base();
    }

    let mut cancelled = false;
    for n in 0..total_frames {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let v = video.as_mut().expect("frame count implies a video stream");
        v.encode(n, n as f64 / fps, &req.tracks, &mut octx)
            .map_err(|e| format!("video encode failed at frame {n}: {e}"))?;
        // Keep audio a frame ahead of the video stream so the muxer's
        // interleaving queue stays shallow instead of buffering the whole run.
        if let Some(a) = audio.as_mut() {
            let want = (((n + 1) as f64 / fps) * SAMPLE_RATE as f64).round() as u64;
            a.advance_to(want.min(total_samples), false, &req.tracks, &mut octx)
                .map_err(|e| format!("audio encode failed at frame {n}: {e}"))?;
        }
        shared.lock().unwrap().progress.done = n + 1;
    }

    // Audio-only renders never entered the loop above, and a video render can
    // still owe a few samples past its last frame boundary.
    if !cancelled {
        if let Some(a) = audio.as_mut() {
            while a.samples_done < total_samples {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                let next = (a.samples_done + a.block as u64).min(total_samples);
                a.advance_to(next, next == total_samples, &req.tracks, &mut octx)
                    .map_err(|e| format!("audio encode failed: {e}"))?;
                if total_frames == 0 {
                    shared.lock().unwrap().progress.done = a.samples_done;
                }
            }
        }
    }

    if cancelled {
        return Ok(false);
    }

    if let Some(v) = video.as_mut() {
        v.enc
            .send_eof()
            .map_err(|e| format!("could not flush the video encoder: {e}"))?;
        drain(&mut v.enc, &mut octx, v.stream_idx, v.enc_tb, v.stream_tb, 1)
            .map_err(|e| format!("could not flush the video encoder: {e}"))?;
    }
    if let Some(a) = audio.as_mut() {
        a.enc
            .send_eof()
            .map_err(|e| format!("could not flush the audio encoder: {e}"))?;
        let block = a.block as i64;
        drain(&mut a.enc, &mut octx, a.stream_idx, a.enc_tb, a.stream_tb, block)
            .map_err(|e| format!("could not flush the audio encoder: {e}"))?;
    }
    octx.write_trailer()
        .map_err(|e| format!("could not finalise the file: {e}"))?;

    Ok(true)
}

/// Pull finished packets out of `enc` and mux them.
///
/// `duration` is in `enc_tb` units and has to be stated explicitly: encoders
/// leave it at zero, and the MP4 muxer then has to infer every sample's length
/// from the gaps between decode timestamps. That inference has no gap to read
/// for the final sample, so the track's edit list ends one frame early and
/// players simply never show the last frame — the file looks fine until you
/// count what comes back out of it.
fn drain(
    enc: &mut encoder::Encoder,
    octx: &mut format::context::Output,
    stream_idx: usize,
    enc_tb: Rational,
    stream_tb: Rational,
    duration: i64,
) -> Result<(), ffmpeg::Error> {
    let mut packet = Packet::empty();
    while enc.receive_packet(&mut packet).is_ok() {
        packet.set_stream(stream_idx);
        packet.set_duration(duration);
        packet.rescale_ts(enc_tb, stream_tb);
        packet.write_interleaved(octx)?;
    }
    Ok(())
}

fn open_video_encoder(
    spec: VideoSpec,
    global_header: bool,
) -> Result<(encoder::Video, Rational), ffmpeg::Error> {
    let codec = encoder::find(codec::Id::H264).ok_or(ffmpeg::Error::EncoderNotFound)?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    // Carrying the frame rate as a rational keeps 29.97 and friends exact;
    // going through a single f64 would drift over a long render.
    let rate = Rational((spec.fps * 1000.0).round() as i32, 1000).reduce();
    let time_base = rate.invert();
    enc.set_width(spec.width);
    enc.set_height(spec.height);
    enc.set_format(format::Pixel::YUV420P);
    enc.set_color_range(ffmpeg::color::Range::MPEG);
    enc.set_time_base(time_base);
    enc.set_frame_rate(Some(rate));
    enc.set_gop((spec.fps * GOP_SECONDS).round().max(1.0) as u32);
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let mut opts = Dictionary::new();
    for (key, value) in X264_OPTS {
        opts.set(key, value);
    }
    Ok((enc.open_with(opts)?, time_base))
}

fn open_audio_encoder(global_header: bool) -> Result<encoder::Audio, ffmpeg::Error> {
    let codec = encoder::find(codec::Id::AAC).ok_or(ffmpeg::Error::EncoderNotFound)?;
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()?;
    // Matching the mixer's own rate and layout means the samples go in
    // untouched — no resampling stage between the mix and the encoder.
    enc.set_rate(SAMPLE_RATE as i32);
    enc.set_channel_layout(ChannelLayout::STEREO);
    enc.set_format(format::Sample::F32(format::sample::Type::Planar));
    enc.set_bit_rate(AUDIO_BIT_RATE);
    enc.set_time_base(Rational(1, SAMPLE_RATE as i32));
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    enc.open_as(codec)
}

fn source_ids(req: &ExportRequest, kind: TrackKind) -> HashSet<SourceId> {
    req.tracks
        .iter()
        .filter(|(k, _)| *k == kind)
        .flat_map(|(_, clips)| clips.iter().map(|c| c.source))
        .collect()
}

fn path_for(req: &ExportRequest, source: SourceId) -> Result<&str, String> {
    req.paths
        .get(&source)
        .map(String::as_str)
        .ok_or_else(|| "a clip refers to media that is no longer loaded".to_string())
}

fn open_video_takes(
    req: &ExportRequest,
    spec: VideoSpec,
) -> Result<HashMap<SourceId, VideoTake>, String> {
    let mut takes = HashMap::new();
    for source in source_ids(req, TrackKind::Video) {
        let path = path_for(req, source)?;
        // Fail the whole export rather than substituting black: silently
        // dropping footage is the one outcome nobody wants to discover later.
        let take = VideoTake::open(path, spec).map_err(|e| format!("cannot read {path}: {e}"))?;
        takes.insert(source, take);
    }
    Ok(takes)
}

fn open_audio_takes(req: &ExportRequest) -> Result<HashMap<SourceId, AudioStream>, String> {
    let mut takes = HashMap::new();
    for source in source_ids(req, TrackKind::Audio) {
        let path = path_for(req, source)?;
        let stream = AudioStream::open(path)
            .map_err(|e| format!("cannot read audio from {path}: {e}"))?
            .ok_or_else(|| format!("{path} has no audio stream"))?;
        takes.insert(source, stream);
    }
    Ok(takes)
}

/// Topmost video clip live at `t`. Higher track index wins, matching what the
/// preview shows.
fn topmost_video_clip(tracks: &[(TrackKind, Vec<Clip>)], t: f64) -> Option<&Clip> {
    tracks
        .iter()
        .rev()
        .filter(|(kind, _)| *kind == TrackKind::Video)
        .find_map(|(_, clips)| clips.iter().find(|c| c.contains(t)))
}

// ---------------------------------------------------------------------------

struct VideoRender {
    enc: encoder::Video,
    enc_tb: Rational,
    stream_idx: usize,
    stream_tb: Rational,
    spec: VideoSpec,
    takes: HashMap<SourceId, VideoTake>,
}

impl VideoRender {
    fn encode(
        &mut self,
        n: u64,
        t: f64,
        tracks: &[(TrackKind, Vec<Clip>)],
        octx: &mut format::context::Output,
    ) -> Result<(), ffmpeg::Error> {
        // A fresh frame per iteration: the encoder keeps a reference to what it
        // is handed, so reusing one buffer would let x264 read a picture we had
        // already started overwriting.
        let mut canvas = frame::Video::new(
            format::Pixel::YUV420P,
            self.spec.width,
            self.spec.height,
        );
        fill_black(&mut canvas);
        if let Some(clip) = topmost_video_clip(tracks, t) {
            if let Some(take) = self.takes.get_mut(&clip.source) {
                if take.stage(clip.source_time(t)) {
                    blit(&mut canvas, &take.staged, take.fit.x, take.fit.y);
                }
            }
        }
        canvas.set_pts(Some(n as i64));
        canvas.set_kind(picture::Type::None);

        self.enc.send_frame(&canvas)?;
        // One tick, because the encoder time base is 1/fps.
        drain(
            &mut self.enc,
            octx,
            self.stream_idx,
            self.enc_tb,
            self.stream_tb,
            1,
        )
    }
}

struct AudioRender {
    enc: encoder::Audio,
    enc_tb: Rational,
    stream_idx: usize,
    stream_tb: Rational,
    takes: HashMap<SourceId, AudioStream>,
    /// Samples per encoder frame — 1024 for AAC.
    block: usize,
    mixed: Vec<f32>,
    scratch: Vec<f32>,
    samples_done: u64,
}

impl AudioRender {
    /// Mix and encode until `target` samples have been produced in total.
    ///
    /// AAC only tolerates a short frame as the very last one, and `target`
    /// tracks video frame boundaries rather than block boundaries, so a partial
    /// tail is held back until `last` says no more samples are coming.
    fn advance_to(
        &mut self,
        target: u64,
        last: bool,
        tracks: &[(TrackKind, Vec<Clip>)],
        octx: &mut format::context::Output,
    ) -> Result<(), ffmpeg::Error> {
        loop {
            let remaining = target.saturating_sub(self.samples_done);
            let frames = if remaining >= self.block as u64 {
                self.block
            } else if last && remaining > 0 {
                remaining as usize
            } else {
                return Ok(());
            };
            let t = self.samples_done as f64 / SAMPLE_RATE as f64;
            self.mix(t, frames, tracks);

            let mut af = frame::Audio::new(
                format::Sample::F32(format::sample::Type::Planar),
                frames,
                ChannelLayout::STEREO,
            );
            af.set_rate(SAMPLE_RATE);
            af.set_pts(Some(self.samples_done as i64));
            for ch in 0..CHANNELS {
                let plane = af.plane_mut::<f32>(ch);
                for i in 0..frames {
                    plane[i] = self.mixed[i * CHANNELS + ch];
                }
            }

            self.enc.send_frame(&af)?;
            // Always a whole block: a short final frame is padded to the
            // codec's frame size on the way out, so the packet still covers
            // `block` samples.
            drain(
                &mut self.enc,
                octx,
                self.stream_idx,
                self.enc_tb,
                self.stream_tb,
                self.block as i64,
            )?;
            self.samples_done += frames as u64;
        }
    }

    /// Sum every audio clip live at `t` into `self.mixed`, each at its own
    /// level. Same rules as live playback: tracks add together so A1 and A2
    /// both land in the render, and each clip's gain and fades ride on top —
    /// the whole reason both mixers call [`Clip::level`] rather than each
    /// deciding what a fade means.
    fn mix(&mut self, t: f64, frames: usize, tracks: &[(TrackKind, Vec<Clip>)]) {
        let wanted = frames * CHANNELS;
        self.mixed[..wanted].fill(0.0);
        for (kind, clips) in tracks {
            if *kind != TrackKind::Audio {
                continue;
            }
            let Some(clip) = clips.iter().find(|c| c.contains(t)) else {
                continue;
            };
            let Some(stream) = self.takes.get_mut(&clip.source) else {
                continue;
            };
            let src_t = clip.source_time(t);
            if (stream.position() - src_t).abs() > RESEEK_THRESHOLD_SEC {
                stream.seek(src_t);
            }
            // Zeroed first because a short read at EOF leaves the tail of
            // `scratch` holding the previous block's samples.
            self.scratch[..wanted].fill(0.0);
            let n = stream.read(&mut self.scratch[..wanted]);
            let dt = 1.0 / SAMPLE_RATE as f64;
            for i in 0..n {
                let level = clip.level(t + (i / CHANNELS) as f64 * dt);
                self.mixed[i] += self.scratch[i] * level;
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// Where a source's picture lands inside the output canvas, preserving its
/// aspect ratio. Everything is even-aligned so the chroma planes, which are
/// half resolution, land on exact sample boundaries.
#[derive(Clone, Copy)]
struct FitRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn fit(src_w: u32, src_h: u32, out_w: u32, out_h: u32) -> FitRect {
    let scale = (out_w as f64 / src_w.max(1) as f64).min(out_h as f64 / src_h.max(1) as f64);
    let w = even(((src_w as f64 * scale).round() as u32).clamp(2, out_w));
    let h = even(((src_h as f64 * scale).round() as u32).clamp(2, out_h));
    FitRect {
        x: even((out_w - w) / 2),
        y: even((out_h - h) / 2),
        w,
        h,
    }
}

fn even(v: u32) -> u32 {
    v & !1
}

fn fill_black(f: &mut frame::Video) {
    for (plane, value) in [(0usize, BLACK_Y), (1, BLACK_UV), (2, BLACK_UV)] {
        let stride = f.stride(plane);
        let rows = f.plane_height(plane) as usize;
        let data = f.data_mut(plane);
        for row in 0..rows {
            data[row * stride..row * stride + stride].fill(value);
        }
    }
}

/// Copy `src` into `dst` at `(x, y)`. Both are YUV420P, and `x`/`y` are even,
/// so the chroma planes copy at exactly half the offset.
fn blit(dst: &mut frame::Video, src: &frame::Video, x: u32, y: u32) {
    for plane in 0..3 {
        let (dx, dy) = if plane == 0 { (x, y) } else { (x / 2, y / 2) };
        let width = src.plane_width(plane) as usize;
        let rows = src.plane_height(plane) as usize;
        let src_stride = src.stride(plane);
        let dst_stride = dst.stride(plane);
        let dst_rows = dst.plane_height(plane) as usize;
        // The fit rect is derived from these same dimensions, but clamp anyway:
        // a source whose frame size changes mid-stream would otherwise index
        // past the canvas.
        let rows = rows.min(dst_rows.saturating_sub(dy as usize));
        let width = width.min(dst_stride.saturating_sub(dx as usize));
        let src_data = src.data(plane);
        let dst_data = dst.data_mut(plane);
        for row in 0..rows {
            let from = row * src_stride;
            let to = (dy as usize + row) * dst_stride + dx as usize;
            dst_data[to..to + width].copy_from_slice(&src_data[from..from + width]);
        }
    }
}

/// One source file, decoded on demand for the render. Mirrors the preview
/// decoder's strategy — walk forward when the next request is close, seek when
/// it is behind or far ahead — but stages frames in CPU memory, already scaled
/// into the output's pixel format.
struct VideoTake {
    ictx: format::context::Input,
    decoder: codec::decoder::Video,
    stream_index: usize,
    time_base_seconds: f64,
    scaler: Option<scaling::Context>,
    /// Decoded but not yet due: the frame after whatever is staged.
    pending: Option<(frame::Video, f64)>,
    /// The staged picture, scaled to `fit` and ready to blit.
    staged: frame::Video,
    staged_pts: Option<f64>,
    fit: FitRect,
}

impl VideoTake {
    fn open(path: &str, spec: VideoSpec) -> Result<Self, ffmpeg::Error> {
        let ictx = format::input(&path)?;
        let (stream_index, time_base_seconds, parameters) = {
            let stream = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .ok_or(ffmpeg::Error::StreamNotFound)?;
            let tb = stream.time_base();
            (
                stream.index(),
                tb.numerator() as f64 / tb.denominator() as f64,
                stream.parameters(),
            )
        };
        let decoder = codec::context::Context::from_parameters(parameters)?
            .decoder()
            .video()?;
        let fit = fit(decoder.width(), decoder.height(), spec.width, spec.height);
        Ok(Self {
            ictx,
            decoder,
            stream_index,
            time_base_seconds,
            scaler: None,
            pending: None,
            staged: frame::Video::empty(),
            staged_pts: None,
            fit,
        })
    }

    /// Make the frame covering source time `t` the staged one. Returns false
    /// only when the source yielded nothing at all, in which case the caller
    /// leaves the canvas black.
    fn stage(&mut self, t: f64) -> bool {
        match self.staged_pts {
            Some(p) if t >= p && t <= p + FORWARD_DECODE_BUDGET => self.advance_to(t),
            _ => self.seek(t),
        }
        self.staged_pts.is_some()
    }

    fn advance_to(&mut self, t: f64) {
        if self.pending.is_none() {
            match self.decode_next() {
                Some(p) => self.pending = Some(p),
                // Past the end of the source: keep showing the last frame,
                // which is what the trim bounds should have prevented anyway.
                None => return,
            }
        }
        if self.pending.as_ref().unwrap().1 > t {
            return;
        }
        loop {
            match self.decode_next() {
                Some((next, next_pts)) => {
                    if next_pts <= t {
                        // The frame we were holding is already superseded, so
                        // it never needs scaling.
                        self.pending = Some((next, next_pts));
                    } else {
                        let (due, due_pts) = self.pending.take().unwrap();
                        self.scale(&due);
                        self.staged_pts = Some(due_pts);
                        self.pending = Some((next, next_pts));
                        return;
                    }
                }
                None => {
                    if let Some((due, due_pts)) = self.pending.take() {
                        self.scale(&due);
                        self.staged_pts = Some(due_pts);
                    }
                    return;
                }
            }
        }
    }

    fn seek(&mut self, t: f64) {
        let ts = (t.max(0.0) * 1_000_000.0) as i64;
        let _ = self.ictx.seek(ts, ..);
        self.decoder.flush();
        self.pending = None;
        self.staged_pts = None;

        let mut last: Option<(frame::Video, f64)> = None;
        loop {
            match self.decode_next() {
                Some((frame, pts)) => {
                    if pts > t {
                        let (due, due_pts) = last.take().unwrap_or_else(|| (frame.clone(), pts));
                        self.scale(&due);
                        self.staged_pts = Some(due_pts);
                        self.pending = Some((frame, pts));
                        return;
                    }
                    last = Some((frame, pts));
                }
                None => {
                    if let Some((due, due_pts)) = last.take() {
                        self.scale(&due);
                        self.staged_pts = Some(due_pts);
                    }
                    return;
                }
            }
        }
    }

    fn decode_next(&mut self) -> Option<(frame::Video, f64)> {
        let mut frame = frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let pts = frame.pts().unwrap_or(0) as f64 * self.time_base_seconds;
                    return Some((frame, pts));
                }
                Err(_) => match self.next_packet() {
                    Some(packet) => {
                        let _ = self.decoder.send_packet(&packet);
                    }
                    None => return None,
                },
            }
        }
    }

    fn next_packet(&mut self) -> Option<ffmpeg::Packet> {
        let mut iter = self.ictx.packets();
        loop {
            let (stream, packet) = iter.next()?;
            if stream.index() == self.stream_index {
                return Some(packet);
            }
        }
    }

    /// Built from the frame rather than the decoder because the decoder's
    /// format is not settled until something has actually come out of it.
    fn scale(&mut self, src: &frame::Video) {
        let scaler = match self.scaler.as_mut() {
            Some(s) => s,
            None => {
                match scaling::Context::get(
                    src.format(),
                    src.width(),
                    src.height(),
                    format::Pixel::YUV420P,
                    self.fit.w,
                    self.fit.h,
                    scaling::Flags::BILINEAR,
                ) {
                    Ok(s) => self.scaler.insert(s),
                    Err(e) => {
                        log::error!("export scaler setup failed: {e}");
                        return;
                    }
                }
            }
        };
        // Re-derive on every frame so a source that changes resolution
        // mid-stream keeps landing in the same output rect.
        scaler.cached(
            src.format(),
            src.width(),
            src.height(),
            format::Pixel::YUV420P,
            self.fit.w,
            self.fit.h,
            scaling::Flags::BILINEAR,
        );
        if let Err(e) = scaler.run(src, &mut self.staged) {
            log::error!("export scale failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_aspect_ratio_fills_the_canvas() {
        let r = fit(1920, 1080, 1280, 720);
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 1280, 720));
    }

    #[test]
    fn a_taller_source_gets_pillarboxed() {
        // 1:1 into 16:9 fits by height, leaving equal bars left and right.
        let r = fit(1000, 1000, 1920, 1080);
        assert_eq!((r.w, r.h), (1080, 1080));
        assert_eq!(r.y, 0);
        assert_eq!(r.x, 420);
    }

    #[test]
    fn a_wider_source_gets_letterboxed() {
        let r = fit(1920, 800, 1920, 1080);
        assert_eq!((r.w, r.h), (1920, 800));
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 140);
    }

    #[test]
    fn every_edge_lands_on_an_even_sample() {
        // Odd dimensions all round: chroma is half resolution, so an odd
        // offset or size would put the planes half a sample out of step.
        let r = fit(1001, 667, 1281, 721);
        for v in [r.x, r.y, r.w, r.h] {
            assert_eq!(v % 2, 0, "{v} is odd");
        }
        assert!(r.x + r.w <= 1281 && r.y + r.h <= 721);
    }

    #[test]
    fn the_source_never_overflows_the_canvas() {
        let r = fit(4000, 3000, 640, 480);
        assert!(r.w <= 640 && r.h <= 480);
    }

    /// End-to-end render, which is the only way to catch a mistake in the
    /// muxing or timestamp handling — the arithmetic above can all be right
    /// while the file still refuses to play. Needs real media, so it takes a
    /// comma-separated list of source files from `RUVE_TEST_MEDIA` and skips
    /// when that is unset rather than carrying a fixture in the repo.
    #[test]
    fn renders_a_two_clip_timeline_to_a_playable_file() {
        let Ok(list) = std::env::var("RUVE_TEST_MEDIA") else {
            return;
        };
        let sources: Vec<&str> = list.split(',').filter(|s| !s.is_empty()).collect();
        assert!(sources.len() >= 2, "RUVE_TEST_MEDIA needs two source files");

        let (a, b) = (SourceId(0), SourceId(1));
        // Ids are unused by the renderer, which walks clips by position; they
        // only matter to the UI's selection.
        let mut next_id = 0;
        let mut clip = |source, source_in, source_out, timeline_start| Clip {
            id: {
                next_id += 1;
                next_id
            },
            source,
            source_in,
            source_out,
            timeline_start,
            ..Clip::default()
        };
        // Two clips butted together, the second starting part-way into its
        // source: that combination exercises the mid-clip seek and the
        // backwards jump at the boundary, which is where a decoder that only
        // walked forward would fall apart.
        let tracks = vec![
            (
                TrackKind::Video,
                vec![clip(a, 0.0, 2.0, 0.0), clip(b, 1.0, 3.0, 2.0)],
            ),
            (
                TrackKind::Audio,
                vec![clip(a, 0.0, 2.0, 0.0), clip(b, 1.0, 3.0, 2.0)],
            ),
        ];
        let paths = HashMap::from([
            (a, sources[0].to_string()),
            (b, sources[1].to_string()),
        ]);
        let output = std::env::temp_dir().join("ruve-export-roundtrip.mp4");
        let spec = VideoSpec {
            width: 640,
            height: 480,
            fps: 25.0,
        };
        let req = ExportRequest {
            output: output.clone(),
            video: Some(spec),
            tracks,
            paths,
        };

        let shared = Mutex::new(Shared {
            progress: Progress::default(),
            outcome: None,
        });
        let cancel = AtomicBool::new(false);
        let completed = run(&req, &shared, &cancel).expect("export failed");
        assert!(completed, "export reported itself cancelled");

        let progress = shared.lock().unwrap().progress;
        assert_eq!(progress.done, progress.total);
        assert_eq!(progress.total, 100, "4s at 25fps");

        ffmpeg::init().unwrap();
        let mut ictx = format::input(&output).expect("output is not readable");
        let (index, parameters) = {
            let stream = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .expect("no video stream in the output");
            (stream.index(), stream.parameters())
        };
        assert!(
            ictx.streams().best(ffmpeg::media::Type::Audio).is_some(),
            "no audio stream in the output"
        );
        let mut decoder = codec::context::Context::from_parameters(parameters)
            .unwrap()
            .decoder()
            .video()
            .unwrap();
        assert_eq!((decoder.width(), decoder.height()), (640, 480));

        // Decoding every frame is the real assertion: a broken timestamp or a
        // truncated trailer shows up here and nowhere else.
        let mut decoded = 0;
        let mut frame = frame::Video::empty();
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            decoder.send_packet(&packet).unwrap();
            while decoder.receive_frame(&mut frame).is_ok() {
                decoded += 1;
            }
        }
        decoder.send_eof().unwrap();
        while decoder.receive_frame(&mut frame).is_ok() {
            decoded += 1;
        }
        assert_eq!(decoded, 100, "every submitted frame should come back out");

        std::fs::remove_file(&output).ok();
    }

    #[test]
    fn progress_reports_a_clamped_fraction() {
        assert_eq!(Progress { done: 0, total: 0 }.fraction(), 0.0);
        assert_eq!(Progress { done: 5, total: 10 }.fraction(), 0.5);
        assert_eq!(Progress { done: 99, total: 10 }.fraction(), 1.0);
    }
}
