//! Export a project file without opening a window: `videoEditor --export
//! edit.vedit out.mp4`. The request is built from the file the way
//! `State::start_export` builds it from the open session, and the same worker
//! renders it, so the file matches what the Export button would write.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use ffmpeg_next as ffmpeg;

use super::{ExportJob, ExportRequest, Outcome, VideoSpec};
use crate::canvas::{Setting, EXPORT_FALLBACK_FPS, EXPORT_FALLBACK_SIZE};
use crate::project::{self, Project, TrackEntry};
use crate::still;
use crate::timeline::{flatten_audio, Clip, SourceId, TrackKind, Tracks};

fn tracks_of(entries: &[TrackEntry]) -> Tracks {
    entries.iter().map(|t| (t.kind, t.clips.clone())).collect()
}

/// Size and average rate of the picture in `path`, as the preview reads them.
fn probe(path: &str) -> Option<((u32, u32), f64)> {
    let ictx = ffmpeg::format::input(&path).ok()?;
    let stream = ictx.streams().best(ffmpeg::media::Type::Video)?;
    let afr = stream.avg_frame_rate();
    let fps = if afr.numerator() > 0 && afr.denominator() > 0 {
        afr.numerator() as f64 / afr.denominator() as f64
    } else {
        30.0
    };
    let p = ffmpeg::codec::context::Context::from_parameters(stream.parameters()).ok()?;
    let v = p.decoder().video().ok()?;
    Some(((v.width(), v.height()), fps))
}

/// Render the project at `project` to `output`, printing progress, and
/// return once the file is written.
pub(crate) fn export(project_path: &Path, output: &Path) -> Result<(), String> {
    ffmpeg::init().map_err(|e| e.to_string())?;
    let loaded: Project =
        project::read(project_path).map_err(|e| format!("{}: {e}", project_path.display()))?;
    let dir = project_path.parent().unwrap_or(Path::new(""));

    // Every source by id: footage and stills by resolved path, titles by
    // value, compounds by their tracks.
    let mut files: HashMap<SourceId, String> = HashMap::new();
    let mut titles = HashMap::new();
    let mut compounds: HashMap<SourceId, Tracks> = HashMap::new();
    for s in &loaded.sources {
        if let Some(title) = &s.title {
            titles.insert(s.id, title.clone());
        } else if let Some(c) = &s.compound {
            compounds.insert(s.id, tracks_of(&c.tracks));
        } else {
            let path = Project::resolve_path(&s.path, dir);
            files.insert(s.id, path.to_string_lossy().into_owned());
        }
    }

    let main: Tracks = tracks_of(&loaded.tracks)
        .into_iter()
        .filter(|(_, clips)| !clips.is_empty())
        .collect();
    if crate::timeline::tracks_duration(&main) <= 0.0 {
        return Err("nothing to export".into());
    }
    let refs: Vec<(TrackKind, &[Clip])> = main.iter().map(|(k, c)| (*k, c.as_slice())).collect();
    let audio = flatten_audio(&refs, &|id| compounds.get(&id).map(Vec::as_slice));
    let tracks: Vec<(TrackKind, Vec<Clip>)> = main
        .iter()
        .filter(|(kind, _)| *kind == TrackKind::Video)
        .cloned()
        .chain(audio.into_iter().map(|clips| (TrackKind::Audio, clips)))
        .collect();

    // The canvas as `State::canvas` finds it: the pinned settings, or the
    // earliest footage on a video track (ties to the lower track).
    let is_footage = |id: SourceId| files.get(&id).is_some_and(|p| !still::is_still(p));
    let reference = main
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| *kind == TrackKind::Video)
        .flat_map(|(ti, (_, clips))| clips.iter().map(move |c| (c.timeline_start, ti, c.source)))
        .filter(|&(_, _, id)| is_footage(id))
        .min_by(|a, b| (a.0, a.1).partial_cmp(&(b.0, b.1)).unwrap())
        .and_then(|(_, _, id)| probe(&files[&id]));
    let (width, height) = match loaded.canvas.resolution {
        Setting::Fixed(size) => size,
        Setting::Auto => reference.map(|r| r.0).unwrap_or(EXPORT_FALLBACK_SIZE),
    };
    let fps = match loaded.canvas.fps {
        Setting::Fixed(fps) => fps,
        Setting::Auto => reference.map(|r| r.1).unwrap_or(0.0),
    };
    let has_picture = main.iter().any(|(k, c)| *k == TrackKind::Video && !c.is_empty());
    let video = has_picture.then(|| VideoSpec {
        width: (width & !1).max(2),
        height: (height & !1).max(2),
        fps: if fps > 0.0 { fps } else { EXPORT_FALLBACK_FPS },
    });

    // Only the compounds the timeline reaches, and every clip in them.
    let mut reached: HashMap<SourceId, Tracks> = HashMap::new();
    let mut every: Vec<Clip> = Vec::new();
    let mut pending: Vec<Clip> = tracks.iter().flat_map(|(_, c)| c.iter().copied()).collect();
    while let Some(clip) = pending.pop() {
        if let Some(inner) = compounds.get(&clip.source) {
            if !reached.contains_key(&clip.source) {
                reached.insert(clip.source, inner.clone());
                pending.extend(inner.iter().flat_map(|(_, c)| c.iter().copied()));
            }
            continue;
        }
        every.push(clip);
    }
    let mut paths = HashMap::new();
    let mut stills = HashMap::new();
    let mut used_titles = HashMap::new();
    for clip in &every {
        if let Some(title) = titles.get(&clip.source) {
            used_titles.insert(clip.source, title.clone());
        } else if let Some(path) = files.get(&clip.source) {
            if still::is_still(path) {
                stills.insert(clip.source, path.clone());
            } else {
                paths.insert(clip.source, path.clone());
            }
        } else {
            return Err(format!("clip {} uses source {}, which is not in the file", clip.id, clip.source.0));
        }
    }

    let job = ExportJob::start(ExportRequest {
        output: output.to_path_buf(),
        video,
        tracks,
        paths,
        titles: used_titles,
        stills,
        compounds: reached,
    });
    loop {
        if let Some(outcome) = job.take_outcome() {
            eprintln!();
            return match outcome {
                Outcome::Done(path) => {
                    println!("wrote {}", path.display());
                    Ok(())
                }
                Outcome::Cancelled => Err("cancelled".into()),
                Outcome::Failed(e) => Err(e),
            };
        }
        eprint!("\r{:3.0}%", job.progress().fraction() * 100.0);
        let _ = std::io::stderr().flush();
        std::thread::sleep(Duration::from_millis(250));
    }
}
