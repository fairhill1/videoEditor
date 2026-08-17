//! Offline render of the timeline to an H.264/AAC file.
//!
//! Deliberately independent of the preview pipeline: the worker opens its own
//! decoders straight from the source paths instead of borrowing the ones behind
//! the media pool. That means an export can run on a background thread while
//! you keep editing, and neither side disturbs the other's seek position.
//!
//! This module is the job itself — the request, the worker thread, the muxer
//! and the progress the UI polls. The work it drives is split by what it acts
//! on: [`raster`] places a picture in the frame and copies planes, [`take`]
//! turns one source into the picture due at a time, and [`streams`] runs the
//! two encode loops over both.

mod raster;
mod streams;
mod take;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, encoder, format, ChannelLayout, Dictionary, Packet, Rational};

use crate::audio::{AudioStream, CHANNELS, SAMPLE_RATE};
use crate::timeline::{Clip, SourceId, TrackKind};
use crate::title::Title;

use streams::{AudioRender, VideoRender};
use take::{TitleTake, VideoTake};

/// x264 knobs. `medium` is the usual speed/size compromise, and crf 20 is
/// visually near-transparent on edited footage without producing huge files.
const X264_OPTS: [(&str, &str); 2] = [("preset", "medium"), ("crf", "20")];
const AUDIO_BIT_RATE: usize = 192_000;
/// Keyframe interval, in seconds. Frequent enough that the result scrubs
/// responsively in a player.
const GOP_SECONDS: f64 = 2.0;
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
    /// The generated sources, which have no file to open and travel by value
    /// instead. Same bargain as `paths`: the worker renders what the timeline
    /// said when the button was pressed, and the session stays editable.
    pub titles: HashMap<SourceId, Title>,
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
            let takes = open_video_takes(req)?;
            let titles = bake_titles(req, spec);
            Some(VideoRender {
                enc,
                enc_tb,
                stream_idx,
                stream_tb: enc_tb,
                spec,
                takes,
                titles,
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
        // Generated sources have no file behind them, so every path this feeds
        // would come back missing.
        .filter(|id| !req.titles.contains_key(id))
        .collect()
}

fn path_for(req: &ExportRequest, source: SourceId) -> Result<&str, String> {
    req.paths
        .get(&source)
        .map(String::as_str)
        .ok_or_else(|| "a clip refers to media that is no longer loaded".to_string())
}

fn open_video_takes(req: &ExportRequest) -> Result<HashMap<SourceId, VideoTake>, String> {
    let mut takes = HashMap::new();
    for source in source_ids(req, TrackKind::Video) {
        let path = path_for(req, source)?;
        // Fail the whole export rather than substituting black: silently
        // dropping footage is the one outcome nobody wants to discover later.
        let take = VideoTake::open(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        takes.insert(source, take);
    }
    Ok(takes)
}

/// Rasterize every title the timeline refers to, once, at the output size.
///
/// Up front rather than on demand because a title's picture never changes
/// during a render — the text is fixed the moment the export starts — so the
/// only thing lazy baking would buy is a branch inside the per-frame loop.
fn bake_titles(req: &ExportRequest, spec: VideoSpec) -> HashMap<SourceId, TitleTake> {
    let used: HashSet<SourceId> = req
        .tracks
        .iter()
        .filter(|(kind, _)| *kind == TrackKind::Video)
        .flat_map(|(_, clips)| clips.iter().map(|c| c.source))
        .collect();
    req.titles
        .iter()
        .filter(|(id, _)| used.contains(id))
        .map(|(id, title)| (*id, TitleTake::bake(title, spec.width, spec.height)))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg::frame;

    use crate::timeline::{SourceId, Transform};

    /// End-to-end render, which is the only way to catch a mistake in the
    /// muxing or timestamp handling — every placement in `raster` can be
    /// right while the file still refuses to play. Needs real media, so it takes a
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
        //
        // Over them, on V2, a title that fades in and out and sits off-centre
        // at half size. That covers the other half of the compositor in the
        // same pass: a second layer, a generated source, a placement that both
        // crops and resamples, and an alpha that is neither 0 nor 1.
        let overlay = SourceId(2);
        let mut title_clip = clip(overlay, 0.0, 3.0, 0.5);
        title_clip.fade_in = 0.5;
        title_clip.fade_out = 0.5;
        title_clip.transform = Transform { x: 0.3, y: -0.25, scale: 0.5 };
        let tracks = vec![
            (
                TrackKind::Video,
                vec![clip(a, 0.0, 2.0, 0.0), clip(b, 1.0, 3.0, 2.0)],
            ),
            (TrackKind::Video, vec![title_clip]),
            (
                TrackKind::Audio,
                vec![clip(a, 0.0, 2.0, 0.0), clip(b, 1.0, 3.0, 2.0)],
            ),
        ];
        let paths = HashMap::from([
            (a, sources[0].to_string()),
            (b, sources[1].to_string()),
        ]);
        let titles = HashMap::from([(
            overlay,
            Title {
                text: "Two\nLines".to_string(),
                ..Title::default()
            },
        )]);
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
            titles,
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

    /// A timeline of nothing but titles is still a picture, and renders as
    /// one. Needs no fixture — there is no file to decode — so unlike the test
    /// above this runs everywhere, and it is the one that covers the generated
    /// path end to end: rasterize, place, blend, encode, and decode back.
    #[test]
    fn renders_a_title_only_timeline() {
        let overlay = SourceId(0);
        let tracks = vec![(
            TrackKind::Video,
            vec![Clip {
                id: 0,
                source: overlay,
                source_out: 2.0,
                fade_in: 0.4,
                fade_out: 0.4,
                ..Clip::default()
            }],
        )];
        let output = std::env::temp_dir().join("ruve-export-title-only.mp4");
        let req = ExportRequest {
            output: output.clone(),
            video: Some(VideoSpec { width: 320, height: 240, fps: 25.0 }),
            tracks,
            paths: HashMap::new(),
            titles: HashMap::from([(overlay, Title::default())]),
        };

        let shared = Mutex::new(Shared {
            progress: Progress::default(),
            outcome: None,
        });
        let completed =
            run(&req, &shared, &AtomicBool::new(false)).expect("title-only export failed");
        assert!(completed, "export reported itself cancelled");

        let mut ictx = format::input(&output).expect("output is not a readable container");
        let stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("no video stream in a title-only render");
        let index = stream.index();
        let mut decoder = codec::context::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .video()
            .unwrap();
        let mut frame = frame::Video::empty();
        // The title is white on black and half way through its hold, so the
        // middle of the render has to carry real ink. A render that placed the
        // mask wrong, or blended it at zero, would decode as an empty frame.
        let mut brightest = 0u8;
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            decoder.send_packet(&packet).unwrap();
            while decoder.receive_frame(&mut frame).is_ok() {
                brightest = brightest.max(frame.data(0).iter().copied().max().unwrap_or(0));
            }
        }
        std::fs::remove_file(&output).ok();
        assert!(brightest > 128, "the title never reached the picture ({brightest})");
    }

    #[test]
    fn progress_reports_a_clamped_fraction() {
        assert_eq!(Progress { done: 0, total: 0 }.fraction(), 0.0);
        assert_eq!(Progress { done: 5, total: 10 }.fraction(), 0.5);
        assert_eq!(Progress { done: 99, total: 10 }.fraction(), 1.0);
    }
}
