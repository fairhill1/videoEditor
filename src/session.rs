//! The live session's relationship to a project file: saving it, opening one,
//! starting a fresh one, and the unsaved-changes bookkeeping that goes with it.
//!
//! `project.rs` owns the on-disk format. This owns the round trip — what gets
//! gathered up on the way out, and what has to be reset on the way in.

use std::collections::HashMap;
use std::path::Path;

use crate::canvas::Setting;
use crate::input::DragMode;
use crate::media::MediaPool;
use crate::project;
use crate::state::{default_tracks, State};
use crate::theme::{STATUS_ERR, STATUS_OK};
use crate::timeline::{Clip, SourceId, Timeline, Track};

impl State {
    /// Name for the title bar and dialogs. Untitled until the project has a
    /// file of its own.
    pub(crate) fn project_display_name(&self) -> &str {
        self.project_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
    }

    /// Keep the window title in step with the project and whether it has
    /// unsaved work. Called once a frame; comparing against `title_shown` is
    /// what keeps that from being a window-manager round trip every frame.
    pub(crate) fn update_title(&mut self) {
        let title = format!(
            "{}{} - videoEditor",
            if self.dirty { "• " } else { "" },
            self.project_display_name()
        );
        if title != self.title_shown {
            self.window.set_title(&title);
            self.title_shown = title;
        }
    }

    /// Ask before throwing unsaved work away; `true` means go ahead. A clean
    /// project never prompts, which is what makes the prompt meaningful when
    /// it does appear.
    pub(crate) fn confirm_discard(&self, action: &str) -> bool {
        if !self.dirty {
            return true;
        }
        let name = self.project_display_name();
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description(format!("{action} will discard unsaved changes to {name}."))
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show()
            == rfd::MessageDialogResult::Ok
    }

    /// Gather the session into a serializable project, with every path stored
    /// relative to `dir` where it can be.
    ///
    /// The whole pool goes in, not just the sources the timeline uses: a bin of
    /// imported footage is part of the project even before it reaches a track,
    /// and reopening to find the unused imports gone would be a quiet loss.
    fn as_project(&self, dir: &Path) -> project::Project {
        let mut ids = self.media.ids().to_vec();
        // A clip whose source has left the pool can't currently arise —
        // removing a pool row removes its clips too — but writing a clip that
        // points at a source the file doesn't carry would make a project that
        // silently drops that clip when reopened. Append rather than trust.
        for track in &self.timeline.tracks {
            for clip in &track.clips {
                if !ids.contains(&clip.source) {
                    ids.push(clip.source);
                }
            }
        }
        project::Project {
            version: project::FORMAT_VERSION,
            canvas: project::CanvasSettings {
                resolution: self.canvas_res,
                fps: self.canvas_fps,
            },
            sources: ids
                .iter()
                .filter_map(|&id| {
                    self.media.get(id).map(|src| project::SourceEntry {
                        id,
                        path: project::Project::store_path(Path::new(&src.path), dir),
                    })
                })
                .collect(),
            tracks: self
                .timeline
                .tracks
                .iter()
                .map(|t| project::TrackEntry {
                    kind: t.kind,
                    clips: t.clips.clone(),
                })
                .collect(),
        }
    }

    /// Write the project, asking for a location when it hasn't got one or when
    /// `save_as` forces the dialog.
    pub(crate) fn save_project(&mut self, save_as: bool) {
        let path = match &self.project_path {
            Some(path) if !save_as => path.clone(),
            _ => {
                let Some(picked) = rfd::FileDialog::new()
                    .add_filter("videoEditor project", &[project::EXTENSION])
                    .set_file_name(format!("untitled.{}", project::EXTENSION))
                    .save_file()
                else {
                    return;
                };
                // A picker the user cleared the suffix in would otherwise
                // produce a file the open dialog's own filter then hides.
                if picked.extension().is_some() {
                    picked
                } else {
                    picked.with_extension(project::EXTENSION)
                }
            }
        };

        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let project = self.as_project(&dir);
        if let Err(e) = project::write(&path, &project) {
            log::error!("failed to save {}: {e}", path.display());
            self.set_status(format!("Save failed: {e}"), STATUS_ERR);
            return;
        }
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        self.project_path = Some(path);
        self.dirty = false;
        self.set_status(format!("Saved {name}"), STATUS_OK);
    }

    pub(crate) fn open_project(&mut self) {
        if !self.confirm_discard("Opening another project") {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("videoEditor project", &[project::EXTENSION])
            .pick_file()
        else {
            return;
        };
        self.load_project(&path);
    }

    /// Replace the session with the project at `path`.
    ///
    /// Media that won't open is reported and its clips are dropped, rather than
    /// refusing the whole file: one moved clip shouldn't make a project
    /// unopenable, and nothing is written back until the next save. There is no
    /// relink UI yet, so saying so loudly is the only warning available.
    pub(crate) fn load_project(&mut self, path: &Path) {
        let loaded = match project::read(path) {
            Ok(loaded) => loaded,
            Err(e) => {
                log::error!("failed to open {}: {e}", path.display());
                self.set_status(format!("Open failed: {e}"), STATUS_ERR);
                return;
            }
        };
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();

        // Re-import into a pool of its own. Ids come from the fresh pool rather
        // than the file, so every clip's source has to be remapped through
        // `imported` — a source that failed to open simply has no entry, which
        // is what identifies the clips that can't be kept.
        let mut media = MediaPool::new();
        let mut imported: HashMap<SourceId, SourceId> = HashMap::new();
        let mut missing = 0;
        for entry in &loaded.sources {
            let resolved = project::Project::resolve_path(&entry.path, &dir);
            match resolved
                .to_str()
                .ok_or(ffmpeg_next::Error::InvalidData)
                .and_then(|p| media.add(p, &self.device, &self.queue, &self.quads))
            {
                Ok(new_id) => {
                    imported.insert(entry.id, new_id);
                }
                Err(e) => {
                    log::error!("missing media {}: {e}", resolved.display());
                    missing += 1;
                }
            }
        }

        let mut dropped = 0;
        let mut tracks = Vec::new();
        for entry in &loaded.tracks {
            let mut track = Track::new(entry.kind);
            for clip in &entry.clips {
                match imported.get(&clip.source) {
                    Some(&source) => track.clips.push(Clip { source, ..*clip }),
                    None => dropped += 1,
                }
            }
            tracks.push(track);
        }

        self.audio.set_playing(false);
        self.audio.set_position(0.0);
        self.media = media;
        self.timeline = Timeline::new();
        self.timeline.tracks = tracks;
        self.timeline.reseed_counters();
        self.canvas_res = loaded.canvas.resolution;
        self.canvas_fps = loaded.canvas.fps;
        self.reset_session_state();
        self.project_path = Some(path.to_path_buf());
        self.dirty = false;

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        if missing == 0 {
            self.set_status(format!("Opened {name}"), STATUS_OK);
        } else {
            self.set_status(
                format!("Opened {name}: {missing} media missing, {dropped} clips dropped"),
                STATUS_ERR,
            );
        }
    }

    /// Start over with an empty timeline. Without this, opening a project would
    /// be a one-way door: nothing else gets you back to a blank session short
    /// of relaunching.
    pub(crate) fn new_project(&mut self) {
        if !self.confirm_discard("Starting a new project") {
            return;
        }
        self.audio.set_playing(false);
        self.audio.set_position(0.0);
        self.media = MediaPool::new();
        self.timeline = Timeline::new();
        self.timeline.tracks = default_tracks();
        self.canvas_res = Setting::Auto;
        self.canvas_fps = Setting::Auto;
        self.reset_session_state();
        self.project_path = None;
        self.dirty = false;
    }

    /// Drop everything that pointed into the timeline that was just replaced.
    /// Undo history is the dangerous one: a step from the previous project
    /// restores clips referencing sources this one has never imported.
    fn reset_session_state(&mut self) {
        self.selected = None;
        self.drag = DragMode::None;
        self.last_playing_source = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.pending_edit = None;
        self.edit_depth = 0;
    }
}
