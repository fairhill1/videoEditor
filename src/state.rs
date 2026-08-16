//! The whole session in one struct, plus the plumbing that owns it: the GPU
//! surface, the media pool, the import path and the export job.
//!
//! `State` is deliberately one flat struct rather than a tree of sub-states.
//! Almost every interaction reads from two or three unrelated corners of it —
//! a drag needs the timeline, the pool and the layout at once — so splitting it
//! would buy encapsulation at the cost of threading borrows through every call.
//! The behaviour is split by module instead: `layout`, `input`, `edit`,
//! `session`, `canvas`, `render` and `toolbar` each add their own `impl State`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use winit::{
    event_loop::OwnedDisplayHandle,
    keyboard::ModifiersState,
    window::{CursorIcon, Window},
};

use crate::audio::AudioEngine;
use crate::canvas::Setting;
use crate::edit::EditSnapshot;
use crate::export::{ExportJob, ExportRequest, Outcome, VideoSpec};
use crate::input::DragMode;
use crate::layout::Splitter;
use crate::media::MediaPool;
use crate::project;
use crate::quad::QuadRenderer;
use crate::text::TextRenderer;
use crate::theme::*;
use crate::timeline::{SourceId, Timeline, Track, TrackKind};
use crate::ui::Rect;

/// V1, V2, A1, A2 — the model supports arbitrary mixes; this is just a sensible
/// starting point so a blank session shows multiple lanes immediately.
pub(crate) fn default_tracks() -> Vec<Track> {
    vec![
        Track::new(TrackKind::Video),
        Track::new(TrackKind::Video),
        Track::new(TrackKind::Audio),
        Track::new(TrackKind::Audio),
    ]
}

fn import_source(
    media: &mut MediaPool,
    path: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    quads: &QuadRenderer,
) {
    if let Err(e) = media.add(path, device, queue, quads) {
        log::error!("failed to load {path}: {e}");
    }
}

pub(crate) struct State {
    pub(crate) instance: wgpu::Instance,
    pub(crate) window: Arc<Window>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    /// Surface size in physical pixels. Only the swapchain and the projection
    /// want this — everything else works through [`State::logical_size`].
    pub(crate) size: winit::dpi::PhysicalSize<u32>,
    /// Physical pixels per logical point, i.e. 2.0 on a Retina display.
    ///
    /// Every layout constant in `theme.rs` is in points, so the UI keeps the
    /// same *physical* size across displays instead of halving on a HiDPI one,
    /// while a bigger window shows more timeline rather than a bigger toolbar.
    /// Nothing multiplies by this directly: the projection is fed a viewport in
    /// points, which makes the conversion the GPU's job. A user zoom
    /// preference, when it arrives, folds in here as a second factor.
    pub(crate) scale: f32,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) surface_format: wgpu::TextureFormat,
    pub(crate) quads: QuadRenderer,
    pub(crate) text: TextRenderer,
    pub(crate) media: MediaPool,
    pub(crate) timeline: Timeline,
    pub(crate) audio: AudioEngine,
    /// In logical points, matching the rects it is tested against.
    pub(crate) cursor: [f32; 2],
    /// Fraction of the window height that sits above the timeline, and fraction
    /// of the width given to the media pool. Read through [`State::timeline_top`]
    /// and [`State::media_pool_w`], which apply the panel minimums.
    pub(crate) split_top_bottom: f32,
    pub(crate) split_pool_preview: f32,
    /// Canvas format, each dimension either pinned or following the footage.
    /// Read together through [`State::canvas`].
    pub(crate) canvas_res: Setting<(u32, u32)>,
    pub(crate) canvas_fps: Setting<f64>,
    pub(crate) project_menu_open: bool,
    /// Popup geometry, filled in while drawing and consumed by the next click.
    /// Storing what was drawn — rather than recomputing the layout to hit-test
    /// it — is what keeps the two from disagreeing about where a row is.
    pub(crate) project_menu_rect: Rect,
    pub(crate) project_menu_items: Vec<(Rect, crate::canvas::ProjectChoice)>,
    /// Last icon handed to the window, so a hover that doesn't change the
    /// cursor doesn't re-set it every frame.
    pub(crate) cursor_icon: CursorIcon,
    pub(crate) drag: DragMode,
    pub(crate) last_playing_source: Option<SourceId>,
    /// Prev-edit, prev-frame, play/pause, next-frame, next-edit — left to
    /// right, so the outer buttons are the coarser jumps.
    pub(crate) transport: [Rect; 5],
    pub(crate) timeline_split_btn: Rect,
    pub(crate) timeline_undo_btn: Rect,
    pub(crate) timeline_redo_btn: Rect,
    pub(crate) timeline_snap_btn: Rect,
    pub(crate) timeline_delete_btn: Rect,
    /// Clip id, not a position — see [`crate::timeline::Clip::id`]. A selection
    /// whose clip has been deleted simply resolves to nothing, and comes back
    /// if an undo restores the clip.
    pub(crate) selected: Option<u32>,
    /// Right-aligned in the toolbar row, well clear of the edit buttons: this
    /// one produces a file rather than changing the timeline.
    pub(crate) timeline_export_btn: Rect,
    pub(crate) timeline_project_btn: Rect,
    /// The project-file pair, left of the gear. Grouped at the right end with
    /// the settings and export buttons because all four concern the project
    /// rather than the timeline, which is what the left cluster edits.
    pub(crate) timeline_open_btn: Rect,
    pub(crate) timeline_save_btn: Rect,
    /// The render in flight, if any. Only one at a time — the button greys out
    /// while it runs, and clicking it again cancels.
    pub(crate) export: Option<ExportJob>,
    /// Outcome of the last thing worth reporting — a render, a save, a failed
    /// open — shown until it ages out.
    pub(crate) status: Option<(String, [f32; 4], Instant)>,
    /// Magnetic snapping while dragging. Toggleable because there is no
    /// timeline zoom yet: on a long timeline the pixel threshold covers a wide
    /// time window, and without an escape hatch a clip could not be parked
    /// near a neighbour without latching onto it.
    pub(crate) snap_enabled: bool,
    pub(crate) pool_open_btn: Rect,
    pub(crate) modifiers: ModifiersState,
    pub(crate) undo_stack: Vec<EditSnapshot>,
    pub(crate) redo_stack: Vec<EditSnapshot>,
    /// State captured before the in-flight edit. Held here rather than pushed
    /// immediately so a drag that fires every mouse-move still collapses into
    /// a single undo step, and so a no-op edit can be discarded.
    pub(crate) pending_edit: Option<EditSnapshot>,
    /// Open `begin_edit` calls. Only the outermost pair produces an undo step,
    /// so a batch operation can wrap self-contained edits and still read as
    /// one Ctrl+Z.
    pub(crate) edit_depth: u32,
    /// Where Ctrl+S writes without asking. `None` until the project has been
    /// saved once or opened from disk.
    pub(crate) project_path: Option<PathBuf>,
    /// Whether anything has changed since the last save. Drives the dot in the
    /// title bar and the prompt on close.
    pub(crate) dirty: bool,
    /// Last string handed to the window manager, so the title is only re-set
    /// when it actually changes rather than every frame.
    pub(crate) title_shown: String,
}

impl State {
    pub(crate) async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        ));
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();

        let size = window.inner_size();

        let surface = instance.create_surface(window.clone()).unwrap();
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        let quads = QuadRenderer::new(&device, &queue, surface_format.add_srgb_suffix());
        let text = TextRenderer::new(&device, &quads);

        let mut timeline = Timeline::new();
        timeline.tracks = default_tracks();

        // A project on the command line replaces the arguments-as-media path
        // entirely, so `videoEditor edit.vedit` opens the edit rather than
        // trying to decode it. Deferred until `State` exists, since loading
        // one is a method on it.
        let args: Vec<String> = std::env::args().skip(1).collect();
        let project_arg = args
            .iter()
            .find(|a| Path::new(a).extension().is_some_and(|e| e == project::EXTENSION))
            .cloned();

        let mut media = MediaPool::new();
        if project_arg.is_none() {
            for path in &args {
                import_source(&mut media, path, &device, &queue, &quads);
            }
        }

        let scale = window.scale_factor() as f32;

        let mut state = State {
            instance,
            window,
            device,
            queue,
            size,
            scale,
            surface,
            surface_format,
            quads,
            text,
            media,
            timeline,
            audio: AudioEngine::new(),
            cursor: [0.0, 0.0],
            split_top_bottom: TOP_BOTTOM_SPLIT,
            split_pool_preview: MEDIA_PREVIEW_SPLIT,
            canvas_res: Setting::Auto,
            canvas_fps: Setting::Auto,
            project_menu_open: false,
            project_menu_rect: Rect::default(),
            project_menu_items: Vec::new(),
            cursor_icon: CursorIcon::Default,
            drag: DragMode::None,
            last_playing_source: None,
            transport: [Rect::default(); 5],
            timeline_split_btn: Rect::default(),
            timeline_undo_btn: Rect::default(),
            timeline_redo_btn: Rect::default(),
            timeline_snap_btn: Rect::default(),
            timeline_delete_btn: Rect::default(),
            selected: None,
            timeline_export_btn: Rect::default(),
            timeline_project_btn: Rect::default(),
            timeline_open_btn: Rect::default(),
            timeline_save_btn: Rect::default(),
            export: None,
            status: None,
            snap_enabled: true,
            pool_open_btn: Rect::default(),
            modifiers: ModifiersState::empty(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_edit: None,
            edit_depth: 0,
            project_path: None,
            dirty: false,
            title_shown: String::new(),
        };

        state.configure_surface();
        state.set_scale(scale);
        if let Some(path) = project_arg {
            state.load_project(Path::new(&path));
        }
        state.update_title();

        state
    }

    pub(crate) fn get_window(&self) -> &Window {
        &self.window
    }

    pub(crate) fn configure_surface(&self) {
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            view_formats: vec![self.surface_format.add_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::AutoVsync,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    pub(crate) fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        self.configure_surface();
    }

    /// Adopt a new device pixel ratio, e.g. after the window is dragged onto a
    /// display with a different one. The surface itself needs no work here:
    /// winit follows a scale change with a `Resized` carrying the new physical
    /// size, which [`State::resize`] handles.
    pub(crate) fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        self.text.set_scale(scale);
    }

    /// Point the cursor at whichever divider it is over or dragging. Dragging
    /// takes priority: once a splitter has been grabbed the cursor keeps its
    /// resize shape even as it runs past the panel's minimum and off the line.
    pub(crate) fn update_cursor_icon(&mut self) {
        let splitter = match self.drag {
            DragMode::Splitter(s) => Some(s),
            DragMode::None => self.splitter_at(self.cursor),
            // Mid-gesture on something else: leave the pointer alone rather
            // than flickering to a resize arrow while a clip is dragged past.
            _ => None,
        };
        let icon = match splitter {
            Some(Splitter::TopBottom) => CursorIcon::RowResize,
            Some(Splitter::PoolPreview) => CursorIcon::ColResize,
            None => CursorIcon::Default,
        };
        if icon != self.cursor_icon {
            self.window.set_cursor(icon);
            self.cursor_icon = icon;
        }
    }

    pub(crate) fn set_status(&mut self, message: String, color: [f32; 4]) {
        self.status = Some((message, color, Instant::now()));
    }

    /// Undoable: an import only adds a pool row, so undo hides it again.
    /// Callers importing a batch should wrap the whole batch in their own
    /// begin/commit pair to get one step for the batch.
    pub(crate) fn import_file(&mut self, path: &str) {
        self.begin_edit();
        import_source(&mut self.media, path, &self.device, &self.queue, &self.quads);
        self.commit_edit();
    }

    pub(crate) fn open_file_picker(&mut self) {
        // Blocking dialog is fine here: a single-user editor pausing the event
        // loop while the OS picker is up is the expected behavior.
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("video", &["mp4", "mov", "mkv", "webm", "avi", "m4v"])
            .pick_files()
        else {
            return;
        };
        // One picker interaction is one undo step, however many files it
        // brought in — the per-file edits nest inside this one.
        self.begin_edit();
        for path in paths {
            if let Some(p) = path.to_str() {
                self.import_file(p);
            }
        }
        self.commit_edit();
    }

    pub(crate) fn can_export(&self) -> bool {
        self.export.is_none() && self.timeline.duration() > 0.0
    }

    /// `None` renders audio-only. A canvas exists either way, but with no video
    /// on the timeline there is nothing to draw on it, and encoding a silent
    /// black picture is not what "export" means here.
    fn export_video_spec(&self) -> Option<VideoSpec> {
        self.reference_video_source()?;
        let canvas = self.canvas();
        Some(VideoSpec {
            width: canvas.width,
            height: canvas.height,
            fps: canvas.fps,
        })
    }

    pub(crate) fn start_export(&mut self) {
        // Clicking Export while one is running cancels it — the button is the
        // only affordance, so it has to be the stop as well as the start.
        if let Some(job) = &self.export {
            job.cancel();
            self.set_status("Cancelling…".into(), STATUS_INFO);
            return;
        }
        if self.timeline.duration() <= 0.0 {
            self.set_status("Nothing to export".into(), STATUS_ERR);
            return;
        }
        let Some(output) = rfd::FileDialog::new()
            .add_filter("MP4 video", &["mp4"])
            .set_file_name("export.mp4")
            .save_file()
        else {
            return;
        };

        // Snapshot the timeline and resolve every path up front, so the worker
        // renders what you saw when you pressed the button and you stay free to
        // keep editing while it runs.
        let tracks = self
            .timeline
            .tracks
            .iter()
            .map(|t| (t.kind, t.clips.clone()))
            .collect::<Vec<_>>();
        let mut paths = HashMap::new();
        for (_, clips) in &tracks {
            for clip in clips {
                if let Some(src) = self.media.get(clip.source) {
                    paths.insert(clip.source, src.path.clone());
                }
            }
        }

        let request = ExportRequest {
            output,
            video: self.export_video_spec(),
            tracks,
            paths,
        };
        self.export = Some(ExportJob::start(request));
        self.set_status("Starting…".into(), STATUS_INFO);
    }

    /// Retire a finished job and turn its result into a status message. Called
    /// once per frame; the worker reports through a mutex rather than the event
    /// loop, so this poll is what surfaces it.
    pub(crate) fn poll_export(&mut self) {
        let Some(job) = &self.export else {
            return;
        };
        let Some(outcome) = job.take_outcome() else {
            return;
        };
        self.export = None;
        match outcome {
            Outcome::Done(path) => {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_string();
                self.set_status(format!("Exported {name}"), STATUS_OK);
            }
            Outcome::Cancelled => {
                self.set_status("Export cancelled".into(), STATUS_INFO);
            }
            Outcome::Failed(err) => {
                log::error!("export failed: {err}");
                self.set_status(format!("Export failed: {err}"), STATUS_ERR);
            }
        }
    }
}
