//! Freeze frame: hold the picture at the playhead for a while, then play on.
//!
//! The held picture is saved as an image beside the project and added to the
//! pool as a still, so from then on it is an ordinary clip: trimmed, moved,
//! zoomed, faded and overlaid like any other, and found again by the export
//! the same way as every other file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;

use crate::colorspace;
use crate::state::State;
use crate::still::Rgba;
use crate::theme::*;
use crate::timeline::{Clip, TrackKind};

/// How long a freeze holds when it is made. The clip can be trimmed from
/// there like any still.
pub(crate) const FREEZE_DURATION: f64 = 2.0;

/// The picture `path` shows at `t` seconds into it, at its own size: the last
/// frame starting at or before `t`, which is the one a player is showing then.
pub(crate) fn grab(path: &str, t: f64) -> Result<Rgba, ffmpeg::Error> {
    let (frame, _, matrix) = frame_at(path, t)?;
    to_rgba(&frame, matrix)
}

/// The frame on screen at `t`, when it starts, and the matrix that converts
/// it.
fn frame_at(
    path: &str,
    t: f64,
) -> Result<(ffmpeg::frame::Video, f64, std::ffi::c_int), ffmpeg::Error> {
    ffmpeg::init()?;
    let mut ictx = ffmpeg::format::input(&path)?;
    let (index, time_base, parameters) = {
        let stream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        let tb = stream.time_base();
        (stream.index(), tb.numerator() as f64 / tb.denominator() as f64, stream.parameters())
    };
    let mut ctx = ffmpeg::codec::context::Context::from_parameters(parameters)?;
    ctx.set_threading(ffmpeg::codec::threading::Config {
        kind: ffmpeg::codec::threading::Type::Frame,
        count: 0,
    });
    let mut decoder = ctx.decoder().video()?;

    // A keyframe at or before `t`, so decoding forward from it passes `t`.
    let ts = (t.max(0.0) * 1_000_000.0) as i64;
    let _ = ictx.seek(ts, ..ts);

    let mut held: Option<(ffmpeg::frame::Video, f64)> = None;
    let mut frame = ffmpeg::frame::Video::empty();
    let mut packets = ictx.packets();
    let mut flushed = false;
    loop {
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let pts = frame.pts().unwrap_or(0) as f64 * time_base;
                // Past `t`: the frame before it is the one on screen, unless
                // there was none, in which case this first one is.
                if pts > t && held.is_some() {
                    break;
                }
                let past = pts > t;
                held = Some((frame.clone(), pts));
                if past {
                    break;
                }
            }
            Err(_) if flushed => break,
            Err(_) => match packets.by_ref().find(|(s, _)| s.index() == index) {
                Some((_, packet)) => {
                    let _ = decoder.send_packet(&packet);
                }
                None => {
                    decoder.send_eof()?;
                    flushed = true;
                }
            },
        }
    }
    let (frame, pts) = held.ok_or(ffmpeg::Error::Eof)?;
    let matrix = colorspace::matrix_for(decoder.color_space(), frame.height());
    Ok((frame, pts, matrix))
}

/// `frame` as straight RGBA, converted with `matrix` as the preview converts
/// it, so the held picture matches the footage either side of it.
fn to_rgba(frame: &ffmpeg::frame::Video, matrix: std::ffi::c_int) -> Result<Rgba, ffmpeg::Error> {
    let (width, height) = (frame.width(), frame.height());
    let mut scaler = ffmpeg::software::scaling::Context::get(
        frame.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )?;
    if !colorspace::use_matrix(&mut scaler, matrix) {
        log::warn!("freeze frame kept the default colour matrix");
    }
    let mut rgba = ffmpeg::frame::Video::empty();
    scaler.run(frame, &mut rgba)?;
    let stride = rgba.stride(0);
    let row = width as usize * 4;
    let mut pixels = vec![0u8; row * height as usize];
    for y in 0..height as usize {
        pixels[y * row..(y + 1) * row].copy_from_slice(&rgba.data(0)[y * stride..y * stride + row]);
    }
    Ok(Rgba { width, height, pixels })
}

/// Where the frames frozen in `project` are kept: a folder beside it named
/// after it, so the project and its pictures move together. Before a project
/// is saved there is nowhere beside it, so they go to the editor's own data
/// folder instead.
pub(crate) fn frames_dir(project: Option<&Path>) -> PathBuf {
    if let Some(project) = project {
        let stem = project.file_stem().and_then(|s| s.to_str()).unwrap_or("project");
        return project.with_file_name(format!("{stem}_frames"));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    data.join("videoEditor").join("frames")
}

/// A file in `dir` for the frame `t` seconds into `source`, not yet taken.
fn frame_path(dir: &Path, source: &str, t: f64) -> PathBuf {
    let stem = Path::new(source).file_stem().and_then(|s| s.to_str()).unwrap_or("frame");
    let base = format!("{stem}_{t:.3}s");
    let mut path = dir.join(format!("{base}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}_{n}.png"));
        n += 1;
    }
    path
}

/// Save `image` as a PNG at `path`. Without its alpha: a frame of footage is
/// opaque, and a file that says so composes as a plain picture.
fn save(image: &Rgba, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let rgb: Vec<u8> = image.pixels.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    image::save_buffer(path, &rgb, image.width, image.height, image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())
}

impl State {
    /// The clip a freeze would hold: the selected footage under the playhead,
    /// or failing that the footage on top there, which is what is on screen.
    pub(crate) fn freeze_target(&self) -> Option<(usize, usize)> {
        let t = self.audio.position();
        let footage = |&(ti, ci): &(usize, usize)| {
            let track = &self.timeline.tracks[ti];
            let clip = &track.clips[ci];
            track.kind == TrackKind::Video
                && clip.contains(t)
                && self.media.get(clip.source).is_some_and(|s| s.stream.is_some())
        };
        let selected = self
            .selected
            .iter()
            .rev()
            .filter_map(|id| self.timeline.find(*id))
            .find(footage);
        // A higher track draws over a lower one, so the topmost is the last.
        selected.or_else(|| {
            self.timeline
                .tracks
                .iter()
                .enumerate()
                .rev()
                .flat_map(|(ti, track)| (0..track.clips.len()).map(move |ci| (ti, ci)))
                .find(footage)
        })
    }

    /// Hold the picture at the playhead for [`FREEZE_DURATION`]: the clip is
    /// cut there, everything after the cut moves later to make room, and a
    /// still of the frame fills the gap. Its sound, if linked, pauses with it;
    /// clips on other tracks that carry on across the playhead — a music bed —
    /// carry on under the freeze.
    pub(crate) fn freeze_frame(&mut self) {
        let Some((ti, ci)) = self.freeze_target() else {
            self.set_status("Put the playhead over footage to freeze it".into(), STATUS_INFO);
            return;
        };
        let t = self.audio.position();
        let clip = self.timeline.tracks[ti].clips[ci];
        let Some(source) = self.media.get(clip.source).map(|s| s.path.clone()) else {
            return;
        };
        let at = clip.source_time(t);
        let image = match grab(&source, at) {
            Ok(image) => image,
            Err(e) => {
                self.set_status(format!("Couldn't read the frame: {e}"), STATUS_ERR);
                return;
            }
        };
        let file = frame_path(&frames_dir(self.project_path.as_deref()), &source, at);
        if let Err(e) = save(&image, &file) {
            self.set_status(format!("Couldn't save the frame: {e}"), STATUS_ERR);
            return;
        }
        let file = file.to_string_lossy().into_owned();

        self.begin_edit();
        let still = match self.media.add(&file, &self.device, &self.queue, &self.quads) {
            Ok(id) => id,
            Err(e) => {
                self.commit_edit();
                self.set_status(format!("Couldn't load the frame: {e}"), STATUS_ERR);
                return;
            }
        };
        let cut: HashSet<u32> = self
            .linked_siblings(ti, ci)
            .into_iter()
            .map(|(t, c)| self.timeline.tracks[t].clips[c].id)
            .collect();
        self.timeline.open_gap(t, FREEZE_DURATION, |c| cut.contains(&c.id));
        let id = self.timeline.new_clip_id();
        self.timeline.tracks[ti].clips.push(Clip {
            id,
            source: still,
            source_in: 0.0,
            source_out: FREEZE_DURATION,
            timeline_start: t,
            opacity: clip.opacity,
            transform: clip.transform,
            effects: clip.effects,
            ..Clip::default()
        });
        self.commit_edit();
        self.selected = vec![id];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame on screen at `t`, found the slow way: every frame from the
    /// start, keeping the last one that has begun by `t`.
    fn straight(path: &str, t: f64) -> (ffmpeg::frame::Video, f64) {
        let mut ictx = ffmpeg::format::input(&path).unwrap();
        let stream = ictx.streams().best(ffmpeg::media::Type::Video).unwrap();
        let (index, tb) = (stream.index(), stream.time_base());
        let tb = tb.numerator() as f64 / tb.denominator() as f64;
        let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .unwrap()
            .decoder()
            .video()
            .unwrap();
        let mut held = None;
        let mut frame = ffmpeg::frame::Video::empty();
        'read: for (s, packet) in ictx.packets() {
            if s.index() != index {
                continue;
            }
            decoder.send_packet(&packet).unwrap();
            while decoder.receive_frame(&mut frame).is_ok() {
                if frame.pts().unwrap_or(0) as f64 * tb > t {
                    break 'read;
                }
                held = Some((frame.clone(), frame.pts().unwrap_or(0) as f64 * tb));
            }
        }
        held.unwrap()
    }

    #[test]
    fn a_grab_is_the_frame_on_screen_at_that_time() {
        let Ok(list) = std::env::var("RUVE_TEST_MEDIA") else {
            eprintln!("RUVE_TEST_MEDIA unset; skipping");
            return;
        };
        let path = list.split(',').next().unwrap();
        // Mid-GOP times, so a seek that lands on the keyframe, or a frame
        // either side of the right one, shows up as the wrong start time.
        for t in [0.0, 1.37, 4.02, 4.035] {
            let (got, got_pts, _) = frame_at(path, t).unwrap();
            let (want, want_pts) = straight(path, t);
            assert_eq!(got_pts, want_pts, "frame at {t}s");
            assert!(got.data(0) == want.data(0), "frame at {t}s differs from a straight decode");
        }
        let image = grab(path, 1.37).unwrap();
        assert_eq!(image.pixels.len(), (image.width * image.height * 4) as usize);
    }

    #[test]
    fn frames_go_beside_the_project_or_to_the_data_folder() {
        let dir = frames_dir(Some(Path::new("/v/RUVE_projects/flare_pool.vedit")));
        assert_eq!(dir, Path::new("/v/RUVE_projects/flare_pool_frames"));
        assert!(frames_dir(None).ends_with("videoEditor/frames"));
    }
}
