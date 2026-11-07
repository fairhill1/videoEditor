//! Compound clips: clips combined into one, which plays as a single picture
//! wherever it is placed, and opens to be edited as a timeline of its own.
//!
//! A compound clip is a pool source like any other — what it holds lives on
//! the source, so every clip of it shows the same contents and editing them
//! once changes them everywhere. Opening one swaps the timeline for its
//! contents, with an undo history of its own; going back writes them into the
//! source and makes the whole visit one step of the outer history.

use std::collections::HashMap;

use crate::edit::EditSnapshot;
use crate::input::DragMode;
use crate::state::State;
use crate::theme::{STATUS_ERR, STATUS_INFO};
use crate::timeline::{
    flatten_audio, tracks_duration, Clip, SourceId, Timeline, TimelineSnapshot, TrackKind, Tracks,
    MAX_NESTING,
};

/// A compound clip opened for editing, and what to put back on leaving it.
pub(crate) struct CompoundEdit {
    pub(crate) source: SourceId,
    /// The timeline that was showing when this one was opened.
    outer: TimelineSnapshot,
    /// Everything as it stood at the moment of opening: the undo step the
    /// visit becomes, if it changed anything.
    before: EditSnapshot,
    undo: Vec<EditSnapshot>,
    redo: Vec<EditSnapshot>,
    playhead: f64,
    view: (f64, f64),
    selected: Vec<u32>,
}

/// The tracks worth keeping in a compound clip: an empty one holds nothing,
/// and the editor adds one of each kind on opening anyway.
fn kept(mut tracks: Tracks) -> Tracks {
    tracks.retain(|(_, clips)| !clips.is_empty());
    tracks
}

impl State {
    /// The compound clip being edited, if the timeline is showing one.
    pub(crate) fn editing_compound(&self) -> Option<SourceId> {
        self.compound_stack.last().map(|e| e.source)
    }

    /// Combine the selected clips, and those linked to them, into one compound
    /// clip: their own tracks and spacing inside it, and on the timeline one
    /// clip in their place, on the lowest video track they used.
    pub(crate) fn combine_selected(&mut self) {
        let ids = self.selection_with_links();
        if ids.is_empty() {
            self.set_status("Select the clips to combine".into(), STATUS_INFO);
            return;
        }
        self.finish_title_edit();
        let picked = || {
            self.timeline
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter())
                .filter(|c| ids.contains(&c.id))
        };
        let start = picked().map(|c| c.timeline_start).fold(f64::INFINITY, f64::min);
        let n = picked().count();
        let contents: Tracks = kept(
            self.timeline
                .tracks
                .iter()
                .map(|t| {
                    let clips = t
                        .clips
                        .iter()
                        .filter(|c| ids.contains(&c.id))
                        .map(|c| Clip { timeline_start: c.timeline_start - start, ..*c })
                        .collect();
                    (t.kind, clips)
                })
                .collect(),
        );
        let duration = tracks_duration(&contents);
        let lowest_video = self.timeline.tracks.iter().position(|t| {
            t.kind == TrackKind::Video && t.clips.iter().any(|c| ids.contains(&c.id))
        });

        self.begin_edit();
        for track in &mut self.timeline.tracks {
            track.clips.retain(|c| !ids.contains(&c.id));
        }
        // The lowest video track the clips were on, if the compound clip
        // fits there now they are gone; otherwise the first video track that
        // is free over the span, or a new one.
        let end = start + duration;
        let free = |t: &crate::timeline::Track| {
            t.kind == TrackKind::Video
                && !t.clips.iter().any(|c| c.timeline_start < end && c.timeline_end() > start)
        };
        let target = lowest_video
            .filter(|&i| free(&self.timeline.tracks[i]))
            .or_else(|| self.timeline.tracks.iter().position(free))
            .unwrap_or_else(|| self.timeline.push_track(TrackKind::Video));
        let number = self.media.ids().iter().filter(|id| self.media.compound(**id).is_some()).count() + 1;
        let name = format!("Compound {number}");
        let source = self.media.add_compound(name.clone(), contents);
        let id = self.timeline.new_clip_id();
        self.timeline.tracks[target].clips.push(Clip {
            id,
            source,
            source_out: duration,
            timeline_start: start,
            ..Clip::default()
        });
        self.commit_edit();
        self.selected = vec![id];
        let noun = if n == 1 { "clip" } else { "clips" };
        self.set_status(format!("Combined {n} {noun} into {name}"), STATUS_INFO);
    }

    /// The compound clip selected, if the selected clip is one.
    pub(crate) fn selected_compound(&self) -> Option<SourceId> {
        let (track, idx) = self.primary_selected()?;
        let source = self.timeline.tracks[track].clips[idx].source;
        self.media.compound(source).map(|_| source)
    }

    /// Open the selected clip's contents, if it is a compound clip.
    pub(crate) fn open_selected_compound(&mut self) {
        let Some((track, idx)) = self.primary_selected() else {
            self.set_status("Select a compound clip to open".into(), STATUS_INFO);
            return;
        };
        let source = self.timeline.tracks[track].clips[idx].source;
        if self.media.compound(source).is_none() {
            self.set_status("That clip is not a compound clip".into(), STATUS_INFO);
            return;
        }
        self.open_compound(source);
    }

    /// Show `source`'s contents as the timeline, to edit them.
    pub(crate) fn open_compound(&mut self, source: SourceId) {
        let Some(contents) = self.media.compound(source).map(<[_]>::to_vec) else {
            return;
        };
        if self.compound_stack.len() >= MAX_NESTING {
            return;
        }
        self.finish_title_edit();
        self.font_menu = None;
        self.drag = DragMode::None;
        let edit = CompoundEdit {
            source,
            outer: self.timeline.snapshot(),
            before: self.edit_snapshot(),
            undo: std::mem::take(&mut self.undo_stack),
            redo: std::mem::take(&mut self.redo_stack),
            playhead: self.audio.position(),
            view: (self.view_start, self.view_dur),
            selected: std::mem::take(&mut self.selected),
        };
        self.compound_stack.push(edit);
        self.pending_edit = None;
        self.edit_depth = 0;
        self.timeline = Timeline::from_tracks(&contents);
        // Somewhere to drop each kind of clip, as a new project has.
        for kind in [TrackKind::Video, TrackKind::Audio] {
            if !self.timeline.tracks.iter().any(|t| t.kind == kind) {
                self.timeline.push_track(kind);
            }
        }
        self.audio.set_position(0.0);
        self.zoom_timeline_to_fit();
    }

    /// Leave the compound clip being edited: its contents go back into it,
    /// and the timeline it was opened from comes back, with the visit one
    /// step of its history.
    pub(crate) fn close_compound(&mut self) {
        self.finish_title_edit();
        let Some(edit) = self.compound_stack.pop() else {
            return;
        };
        self.font_menu = None;
        self.drag = DragMode::None;
        self.pending_edit = None;
        self.edit_depth = 0;
        let contents = kept(self.timeline.to_tracks());
        self.media.set_compound(edit.source, contents);
        self.timeline.restore(&edit.outer);
        self.undo_stack = edit.undo;
        self.redo_stack = edit.redo;
        if self.edit_snapshot() != edit.before {
            self.undo_stack.push(edit.before);
            self.redo_stack.clear();
            self.dirty = true;
        }
        self.selected = edit.selected;
        (self.view_start, self.view_dur) = edit.view;
        let duration = self.timeline.duration();
        self.audio.set_position(edit.playhead.clamp(0.0, duration.max(0.0)));
    }

    /// Whether a clip of `source` may go on the timeline showing now: not
    /// when it is, or holds, a compound clip open around it — a compound clip
    /// inside itself would have no end.
    pub(crate) fn may_place(&self, source: SourceId) -> bool {
        self.compound_stack
            .iter()
            .all(|e| !self.media.contains_compound(source, e.source))
    }

    /// The project as it would be if every open compound clip were closed
    /// now: the outermost timeline, and the contents of every compound clip
    /// with the ones being edited taken from the editor. What saving and
    /// exporting see, so neither has to leave the compound clip first.
    pub(crate) fn committed(&self) -> (Tracks, HashMap<SourceId, Tracks>) {
        let mut current = self.timeline.to_tracks();
        let mut edited = HashMap::new();
        for edit in self.compound_stack.iter().rev() {
            edited.insert(edit.source, kept(current));
            current = edit.outer.tracks().to_vec();
        }
        let mut all: HashMap<SourceId, Tracks> = self.media.compound_snapshot().into_iter().collect();
        all.extend(edited);
        (current, all)
    }

    /// The audio tracks of `tracks`, with the sound inside their compound
    /// clips moved onto their clock, as the export plays them.
    pub(crate) fn flattened_audio(
        tracks: &Tracks,
        compounds: &HashMap<SourceId, Tracks>,
    ) -> Vec<Vec<Clip>> {
        let refs: Vec<(TrackKind, &[Clip])> =
            tracks.iter().map(|(k, c)| (*k, c.as_slice())).collect();
        flatten_audio(&refs, &|id| compounds.get(&id).map(Vec::as_slice))
    }

    /// Say why a drop or paste of a compound clip into itself did nothing.
    pub(crate) fn refuse_nesting(&mut self) {
        self.set_status("A compound clip can't go inside itself".into(), STATUS_ERR);
    }
}
