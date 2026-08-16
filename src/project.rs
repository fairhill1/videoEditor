//! The project file: what a session is worth keeping across a restart.
//!
//! Three things go in — the canvas settings, the media the timeline references,
//! and the timeline itself. Everything else on `State` is either derived (the
//! decoded frames, the waveforms) or a view preference that belongs to the
//! window rather than the project (panel splits, playhead, undo history).
//!
//! RON rather than JSON because the data is enum-shaped: `Setting::Auto` and
//! `Clip::link` round-trip as `Auto` and `Some(3)` instead of the tagged-object
//! and `null` encodings JSON would force, and the result stays readable enough
//! to hand-edit or review in a diff.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Setting;
use crate::timeline::{Clip, SourceId, TrackKind};

/// Written into every file and checked on the way back in.
///
/// Refusing to open a version this build doesn't know is the point: a
/// half-understood file that then gets saved back over would silently drop
/// whatever the newer format carried.
pub const FORMAT_VERSION: u32 = 1;

pub const EXTENSION: &str = "vedit";

/// Prepended on write and skipped by the parser on read, so someone who opens
/// the file in an editor knows what they're looking at.
const HEADER: &str = "// videoEditor project file.\n\
                      // Paths are relative to this file where the media sits alongside it.\n";

#[derive(Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub canvas: CanvasSettings,
    pub sources: Vec<SourceEntry>,
    pub tracks: Vec<TrackEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct CanvasSettings {
    pub resolution: Setting<(u32, u32)>,
    pub fps: Setting<f64>,
}

#[derive(Serialize, Deserialize)]
pub struct SourceEntry {
    /// The id this source had in the saving session. Meaningless on its own —
    /// a load re-imports the files and gets fresh ids — but it's what the
    /// clips below point at, so it has to survive the round trip to be
    /// remapped.
    pub id: SourceId,
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct TrackEntry {
    pub kind: TrackKind,
    pub clips: Vec<Clip>,
}

/// Note the absence of the clip and link id counters. They're derivable from
/// the clips themselves ([`crate::timeline::Timeline::reseed_counters`]), and
/// deriving beats storing: a hand-edited file can't seed a counter that would
/// hand out an id some clip already holds.
impl Project {
    /// Where `media` should be recorded in a project file living at
    /// `project_dir`.
    ///
    /// Media under the project's own directory is stored relative to it, so a
    /// folder holding the project and its footage can be moved or copied
    /// wholesale and still open. Anything outside stays absolute — walking up
    /// with `..` would make the file depend on the two paths keeping their
    /// relative arrangement, which is a weaker guarantee than an absolute path,
    /// not a stronger one.
    pub fn store_path(media: &Path, project_dir: &Path) -> String {
        media
            .strip_prefix(project_dir)
            .unwrap_or(media)
            .to_string_lossy()
            .into_owned()
    }

    /// Inverse of [`Project::store_path`].
    pub fn resolve_path(stored: &str, project_dir: &Path) -> PathBuf {
        let stored = Path::new(stored);
        if stored.is_absolute() {
            stored.to_path_buf()
        } else {
            project_dir.join(stored)
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Parse(ron::error::SpannedError),
    Encode(ron::Error),
    /// The file parsed, but as a format this build doesn't know.
    Version(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Parse(e) => write!(f, "{e}"),
            Error::Encode(e) => write!(f, "{e}"),
            Error::Version(v) => write!(
                f,
                "project format v{v}, this build reads v{FORMAT_VERSION}"
            ),
        }
    }
}

pub fn write(path: &Path, project: &Project) -> Result<(), Error> {
    let config = ron::ser::PrettyConfig::default();
    let body = ron::ser::to_string_pretty(project, config).map_err(Error::Encode)?;
    std::fs::write(path, format!("{HEADER}{body}\n")).map_err(Error::Io)
}

pub fn read(path: &Path) -> Result<Project, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::Io)?;
    let project: Project = ron::from_str(&text).map_err(Error::Parse)?;
    if project.version != FORMAT_VERSION {
        return Err(Error::Version(project.version));
    }
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Project {
        Project {
            version: FORMAT_VERSION,
            canvas: CanvasSettings {
                resolution: Setting::Fixed((1920, 1080)),
                fps: Setting::Auto,
            },
            sources: vec![SourceEntry {
                id: SourceId(0),
                path: "a.mp4".into(),
            }],
            tracks: vec![TrackEntry {
                kind: TrackKind::Video,
                clips: vec![Clip {
                    id: 0,
                    source: SourceId(0),
                    source_in: 1.5,
                    source_out: 4.5,
                    timeline_start: 0.0,
                    link: Some(2),
                }],
            }],
        }
    }

    fn round_trip(project: &Project) -> Project {
        let text = ron::ser::to_string_pretty(project, ron::ser::PrettyConfig::default()).unwrap();
        ron::from_str(&text).unwrap()
    }

    #[test]
    fn a_project_survives_the_round_trip() {
        let back = round_trip(&sample());
        assert_eq!(back.canvas.resolution, Setting::Fixed((1920, 1080)));
        assert_eq!(back.canvas.fps, Setting::Auto);
        assert_eq!(back.sources[0].path, "a.mp4");
        assert_eq!(back.tracks[0].kind, TrackKind::Video);
        assert_eq!(back.tracks[0].clips[0], sample().tracks[0].clips[0]);
    }

    /// Trim points are the one field where a lossy round trip would be
    /// invisible until an export drifted out of sync, so pin the exactness.
    #[test]
    fn trim_points_survive_to_the_last_bit() {
        let mut project = sample();
        project.tracks[0].clips[0].source_in = 1.0 / 3.0;
        project.tracks[0].clips[0].timeline_start = 0.1 + 0.2;
        let back = round_trip(&project);
        assert_eq!(back.tracks[0].clips[0].source_in, 1.0 / 3.0);
        assert_eq!(back.tracks[0].clips[0].timeline_start, 0.1 + 0.2);
    }

    /// The parser has to skip the header we prepend, or every file we write is
    /// one we can't read.
    #[test]
    fn the_header_comment_is_not_part_of_the_data() {
        let body =
            ron::ser::to_string_pretty(&sample(), ron::ser::PrettyConfig::default()).unwrap();
        let parsed: Project = ron::from_str(&format!("{HEADER}{body}\n")).unwrap();
        assert_eq!(parsed.version, FORMAT_VERSION);
    }

    /// Unique per test so the cases can run in parallel, and named after the
    /// test so a leftover file says which one left it.
    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vedit-{}-{tag}.{EXTENSION}", std::process::id()))
    }

    /// The round-trip tests above go through the string; this one goes through
    /// the filesystem, which is what covers the header and the version gate.
    #[test]
    fn a_written_file_reads_back() {
        let path = temp_file("write-read");
        write(&path, &sample()).unwrap();
        let back = read(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(back.version, FORMAT_VERSION);
        assert_eq!(back.canvas.resolution, Setting::Fixed((1920, 1080)));
        assert_eq!(back.tracks[0].clips[0], sample().tracks[0].clips[0]);
    }

    #[test]
    fn a_future_version_is_refused_rather_than_half_read() {
        let path = temp_file("future");
        let mut project = sample();
        project.version = FORMAT_VERSION + 1;
        write(&path, &project).unwrap();
        let result = read(&path);
        std::fs::remove_file(&path).ok();

        assert!(matches!(result, Err(Error::Version(v)) if v == FORMAT_VERSION + 1));
    }

    #[test]
    fn a_file_that_is_not_a_project_fails_to_parse() {
        let path = temp_file("garbage");
        std::fs::write(&path, "this is not RON").unwrap();
        let result = read(&path);
        std::fs::remove_file(&path).ok();

        assert!(matches!(result, Err(Error::Parse(_))));
    }

    #[test]
    fn media_beside_the_project_is_stored_relative() {
        let dir = Path::new("/work/edit");
        assert_eq!(
            Project::store_path(Path::new("/work/edit/footage/a.mp4"), dir),
            "footage/a.mp4"
        );
        assert_eq!(
            Project::resolve_path("footage/a.mp4", dir),
            PathBuf::from("/work/edit/footage/a.mp4")
        );
    }

    #[test]
    fn media_outside_the_project_stays_absolute() {
        let dir = Path::new("/work/edit");
        let outside = Path::new("/Volumes/card/a.mp4");
        let stored = Project::store_path(outside, dir);
        assert_eq!(stored, "/Volumes/card/a.mp4");
        assert_eq!(Project::resolve_path(&stored, dir), outside);
    }
}
