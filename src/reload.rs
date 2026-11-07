//! The project file changing under the open session: noticing it, and taking
//! the change in place.
//!
//! The file is meant to be editable from outside — by hand, or by a tool
//! working alongside the editor — and seeing that edit should not mean
//! reopening the project. So the open file is watched, and a change to it is
//! merged in as one undo step: media already decoded stays decoded, the
//! playhead, zoom and window stay where they are, and Ctrl+Z puts things back.
//!
//! Unsaved work is never replaced without asking. With some, the change waits
//! and the toolbar's reload button lights up; saving instead keeps yours.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::input::DragMode;
use crate::project;
use crate::state::State;
use crate::theme::{STATUS_ERR, STATUS_INFO, STATUS_OK};
use crate::timeline::{Clip, SourceId, Track};

/// How often the file is looked at. A stat call, so cheap; this is about how
/// soon a change shows up.
const POLL_EVERY: Duration = Duration::from_millis(400);

/// What the session knows of the file on disk.
#[derive(Default)]
pub(crate) struct DiskState {
    /// The file's size and time as last seen. A change to either means it is
    /// worth reading again.
    stamp: Option<(SystemTime, u64)>,
    /// What the file said when the session last matched it — loaded from it
    /// or saved to it. A change that leaves it the same is the session's own.
    known: Option<u64>,
    /// Ids in the file, by the ids the session gave the same sources. Saving
    /// writes the session's own, so after a save this is empty and every id
    /// is itself.
    ids: HashMap<SourceId, SourceId>,
    /// The file has a change the session has not taken, because taking it
    /// would have replaced unsaved work.
    pub(crate) pending: bool,
    checked: Option<Instant>,
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn digest(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// `path` with `..` and links resolved, for telling whether two paths name
/// the same file. As given when it cannot be resolved.
fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl State {
    /// Remember the file as it is now as matching the session: called after
    /// loading it and after saving it. `ids` maps the file's ids to the
    /// session's.
    pub(crate) fn note_disk(&mut self, ids: HashMap<SourceId, SourceId>) {
        let Some(path) = self.project_path.clone() else {
            self.disk = DiskState::default();
            return;
        };
        let text = std::fs::read_to_string(&path).ok();
        self.disk = DiskState {
            stamp: stamp(&path),
            known: text.as_deref().map(digest),
            ids,
            pending: false,
            checked: Some(Instant::now()),
        };
    }

    /// Look at the file, now and then, and take a change made to it from
    /// outside. Called every frame; does nothing most of them.
    pub(crate) fn poll_disk(&mut self) {
        if self.disk.checked.is_some_and(|t| t.elapsed() < POLL_EVERY) {
            return;
        }
        self.disk.checked = Some(Instant::now());
        let Some(path) = self.project_path.clone() else {
            return;
        };
        let now = stamp(&path);
        if now.is_none() || now == self.disk.stamp {
            return;
        }
        // Not in the middle of a gesture or of typing: the change waits for
        // it, rather than pulling the clip being dragged out from under it.
        if !matches!(self.drag, DragMode::None) || self.typing_title() || self.typing_hex() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        self.disk.stamp = now;
        if Some(digest(&text)) == self.disk.known {
            return;
        }
        if self.dirty {
            if !self.disk.pending {
                self.disk.pending = true;
                self.set_status(
                    format!(
                        "{} changed on disk. Reload to take it, or save to keep yours",
                        self.project_display_name()
                    ),
                    STATUS_INFO,
                );
            }
            return;
        }
        self.reload_project();
    }

    /// Take the file as it now stands into the session, as one undo step.
    pub(crate) fn reload_project(&mut self) {
        let Some(path) = self.project_path.clone() else {
            return;
        };
        let loaded = match project::read(&path) {
            Ok(loaded) => loaded,
            Err(e) => {
                // Left as it was, and said loudly: the file is what is wrong,
                // and the next change to it is looked at again.
                self.set_status(format!("Couldn't reload: {e}"), STATUS_ERR);
                return;
            }
        };
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();

        self.close_color_picker();
        self.finish_title_edit();
        self.font_menu = None;
        while !self.compound_stack.is_empty() {
            self.close_compound();
        }
        self.drag = DragMode::None;
        let before = self.edit_snapshot();

        // Each source in the file, as a source of the session's: the same
        // one where it already has it, so nothing already decoded is opened
        // again, and a new one where it does not.
        let mut ids: HashMap<SourceId, SourceId> = HashMap::new();
        let mut missing = 0;
        for entry in &loaded.sources {
            let known = self.disk.ids.get(&entry.id).copied().unwrap_or(entry.id);
            if let Some(title) = entry.title.clone() {
                let id = if self.media.title(known).is_some() {
                    self.media.set_title(known, title);
                    known
                } else {
                    self.media.add_title(title)
                };
                ids.insert(entry.id, id);
                continue;
            }
            if let Some(compound) = entry.compound.as_ref() {
                let id = if self.media.compound(known).is_some() {
                    known
                } else {
                    self.media.add_compound(compound.name.clone(), Vec::new())
                };
                if let Some(src) = self.media.get_mut(id) {
                    src.name = compound.name.clone();
                }
                ids.insert(entry.id, id);
                continue;
            }
            let resolved = canonical(&project::Project::resolve_path(&entry.path, &dir));
            let same = |id: SourceId| {
                self.media.get(id).is_some_and(|s| {
                    s.title.is_none() && s.compound.is_none() && canonical(Path::new(&s.path)) == resolved
                })
            };
            let found = if same(known) {
                Some(known)
            } else {
                self.media.all_ids().into_iter().find(|&id| same(id))
            };
            let id = match found {
                Some(id) => Some(id),
                None => resolved
                    .to_str()
                    .and_then(|p| self.media.add(p, &self.device, &self.queue, &self.quads).ok()),
            };
            match id {
                Some(id) => {
                    ids.insert(entry.id, id);
                }
                None => {
                    log::error!("missing media {}", resolved.display());
                    missing += 1;
                }
            }
        }

        let remap = |clips: &[Clip]| -> Vec<Clip> {
            clips
                .iter()
                .filter_map(|c| ids.get(&c.source).map(|&source| Clip { source, ..*c }))
                .collect()
        };
        for entry in &loaded.sources {
            if let (Some(compound), Some(&id)) = (entry.compound.as_ref(), ids.get(&entry.id)) {
                let tracks = compound.tracks.iter().map(|t| (t.kind, remap(&t.clips))).collect();
                self.media.set_compound(id, tracks);
            }
        }
        self.timeline.tracks = loaded
            .tracks
            .iter()
            .map(|t| {
                let mut track = Track::new(t.kind);
                track.clips = remap(&t.clips);
                track
            })
            .collect();
        self.timeline.reseed_counters();
        let order: Vec<SourceId> = loaded.sources.iter().filter_map(|e| ids.get(&e.id).copied()).collect();
        self.media.set_order(&order);
        self.canvas_res = loaded.canvas.resolution;
        self.canvas_fps = loaded.canvas.fps;

        let alive = |id: &u32| self.timeline.find(*id).is_some();
        let selected: Vec<u32> = self.selected.iter().copied().filter(alive).collect();
        self.selected = selected;
        let duration = self.timeline.duration();
        if self.audio.position() > duration {
            self.audio.set_position(duration.max(0.0));
        }
        self.last_playing_source = None;

        self.push_step(before);
        self.dirty = false;
        self.note_disk(ids);
        let name = self.project_display_name().to_string();
        if missing == 0 {
            self.set_status(format!("Reloaded {name} from disk (Ctrl+Z to undo)"), STATUS_OK);
        } else {
            self.set_status(format!("Reloaded {name}: {missing} media missing"), STATUS_ERR);
        }
    }
}

/// Read the project at `path` and say whether it would open cleanly: what is
/// in it, and anything that would be dropped or substituted. For checking a
/// file edited outside the editor before the editor sees it.
pub(crate) fn check(path: &Path) -> Result<String, String> {
    let loaded = project::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut problems = Vec::new();
    let ids: Vec<SourceId> = loaded.sources.iter().map(|s| s.id).collect();
    let mut seen = std::collections::HashSet::new();
    for s in &loaded.sources {
        if !seen.insert(s.id) {
            problems.push(format!("source id {} is used twice", s.id.0));
        }
        if let Some(title) = &s.title {
            if let Some(font) = &title.font {
                if !crate::fonts::families().iter().any(|f| f == font) {
                    problems.push(format!("font {font:?} is not installed; the default face is used"));
                }
            }
        } else if s.compound.is_none() {
            let resolved = project::Project::resolve_path(&s.path, dir);
            if !resolved.exists() {
                problems.push(format!("media {} does not exist", resolved.display()));
            }
        }
    }
    let mut clip_ids = std::collections::HashSet::new();
    let mut clips = 0;
    let mut look = |where_: &str, clip: &Clip, problems: &mut Vec<String>| {
        clips += 1;
        if !ids.contains(&clip.source) {
            problems.push(format!("{where_}: clip {} uses source {}, which is not in the file", clip.id, clip.source.0));
        }
        if clip.source_out <= clip.source_in {
            problems.push(format!("{where_}: clip {} ends before it starts", clip.id));
        }
        if !clip.timeline_start.is_finite() || clip.timeline_start < 0.0 {
            problems.push(format!("{where_}: clip {} starts at {}", clip.id, clip.timeline_start));
        }
    };
    for t in &loaded.tracks {
        for c in &t.clips {
            if !clip_ids.insert(c.id) {
                problems.push(format!("timeline: clip id {} is used twice", c.id));
            }
            look("timeline", c, &mut problems);
        }
    }
    for s in &loaded.sources {
        if let Some(compound) = &s.compound {
            for t in &compound.tracks {
                for c in &t.clips {
                    look(&format!("compound {:?}", compound.name), c, &mut problems);
                }
            }
        }
    }
    let summary = format!("{} sources, {} tracks, {clips} clips", loaded.sources.len(), loaded.tracks.len());
    if problems.is_empty() {
        Ok(format!("ok: {summary}"))
    } else {
        Err(format!("{summary}\n{}", problems.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Setting;
    use crate::project::{CanvasSettings, Project, SourceEntry, TrackEntry};
    use crate::timeline::TrackKind;
    use crate::title::Title;

    fn write(name: &str, sources: Vec<SourceEntry>, clips: Vec<Clip>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ruve-check-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.vedit");
        let p = Project {
            version: project::FORMAT_VERSION,
            canvas: CanvasSettings { resolution: Setting::Auto, fps: Setting::Auto },
            sources,
            tracks: vec![TrackEntry { kind: TrackKind::Video, clips }],
            layout: None,
        };
        project::write(&path, &p).unwrap();
        path
    }

    fn title(id: u32) -> SourceEntry {
        SourceEntry { id: SourceId(id), path: String::new(), title: Some(Title::default()), compound: None }
    }

    fn clip(id: u32, source: u32) -> Clip {
        Clip { id, source: SourceId(source), source_out: 5.0, ..Clip::default() }
    }

    #[test]
    fn a_sound_file_checks_out() {
        let path = write("ok", vec![title(1)], vec![clip(0, 1)]);
        assert_eq!(check(&path), Ok("ok: 1 sources, 1 tracks, 1 clips".into()));
    }

    #[test]
    fn a_clip_of_a_missing_source_and_a_repeated_id_are_reported() {
        let path = write("bad", vec![title(1)], vec![clip(0, 1), clip(0, 9)]);
        let problems = check(&path).unwrap_err();
        assert!(problems.contains("uses source 9"), "{problems}");
        assert!(problems.contains("clip id 0 is used twice"), "{problems}");
    }

    #[test]
    fn a_file_that_does_not_parse_says_so() {
        let path = write("parse", vec![], vec![]);
        std::fs::write(&path, "(version: 1, nonsense").unwrap();
        assert!(check(&path).is_err());
    }
}
