mod audio;
mod media;
mod quad;
mod text;
mod timeline;
mod video;

use std::sync::Arc;

use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use audio::AudioEngine;
use media::MediaPool;
use quad::{Quad, QuadRenderer};
use text::TextRenderer;
use timeline::{Clip, SourceId, Timeline, Track, TrackKind};

// Layout split ratios — tweak to taste.
const TOP_BOTTOM_SPLIT: f32 = 0.55;
const MEDIA_PREVIEW_SPLIT: f32 = 0.28;

// Panel colors (sRGB).
const MEDIA_POOL_COLOR: [f32; 4] = [0.14, 0.14, 0.16, 1.0];
const PREVIEW_COLOR: [f32; 4] = [0.04, 0.04, 0.05, 1.0];
const TIMELINE_COLOR: [f32; 4] = [0.10, 0.10, 0.12, 1.0];
const LANE_COLOR: [f32; 4] = [0.07, 0.07, 0.09, 1.0];
const DIVIDER_COLOR: [f32; 4] = [0.20, 0.20, 0.22, 1.0];
const VIDEO_CLIP_COLOR: [f32; 4] = [0.30, 0.45, 0.70, 1.0];
const AUDIO_CLIP_COLOR: [f32; 4] = [0.30, 0.60, 0.40, 1.0];
const AUDIO_WAVE_COLOR: [f32; 4] = [0.75, 0.95, 0.80, 0.95];
const CLIP_LABEL_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const LABEL_COLOR: [f32; 4] = [0.65, 0.65, 0.70, 1.0];
const TRACK_LABEL_COLOR: [f32; 4] = [0.45, 0.45, 0.50, 1.0];
const LABEL_SIZE: f32 = 13.0;
const CLIP_LABEL_SIZE: f32 = 11.0;
const LABEL_PAD: f32 = 10.0;
const PLAYHEAD_COLOR: [f32; 4] = [0.95, 0.35, 0.35, 1.0];
const PLAYHEAD_WIDTH: f32 = 2.0;
const TIMER_SIZE: f32 = 14.0;
const TIMER_PAD: f32 = 12.0;
const TIMER_COLOR: [f32; 4] = [0.95, 0.95, 0.98, 1.0];
const TIMER_BG_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.55];

// Timeline panel layout.
// Lane height is computed per-frame to fill the timeline area; these bounds
// keep it readable with one track and prevent chunkiness at high counts.
const TRACK_LANE_MIN_H: f32 = 32.0;
const TRACK_LANE_MAX_H: f32 = 88.0;
const TRACK_LANE_FILL: f32 = 0.9; // fraction of tracks-area height the lanes+gaps try to fill
const TRACK_LANE_GAP: f32 = 2.0;
const TRACK_HEADER_WIDTH: f32 = 48.0;
const TIMELINE_TOP_PAD: f32 = 30.0; // clear space for the "TIMELINE" label

// Media pool list layout.
const POOL_LIST_TOP: f32 = 36.0; // below the MEDIA POOL label
const POOL_ROW_HEIGHT: f32 = 64.0;
const POOL_ROW_GAP: f32 = 4.0;
const POOL_ROW_PAD: f32 = 6.0;
const POOL_ROW_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
const POOL_ITEM_NAME_SIZE: f32 = 12.0;
const POOL_ITEM_META_SIZE: f32 = 10.0;
// Thumbnail slot inside each row — fixed ~16:9 slot, actual thumb is
// letterboxed into it preserving source aspect.
const POOL_THUMB_W: f32 = 92.0;
const POOL_THUMB_H: f32 = POOL_ROW_HEIGHT - POOL_ROW_PAD * 2.0;
const POOL_THUMB_BG: [f32; 4] = [0.08, 0.08, 0.10, 1.0];
const POOL_DUR_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.65];
const POOL_DUR_TEXT: [f32; 4] = [0.95, 0.95, 0.98, 1.0];

// Clip interaction.
const CLIP_EDGE_GRAB_PX: f32 = 6.0;
const MIN_CLIP_DURATION: f64 = 0.05; // seconds — keeps trim from zeroing a clip
const DRAG_GHOST_VIDEO_COLOR: [f32; 4] = [0.30, 0.45, 0.70, 0.55];
const DRAG_GHOST_AUDIO_COLOR: [f32; 4] = [0.30, 0.60, 0.40, 0.55];

#[derive(Copy, Clone, Debug)]
enum DragMode {
    None,
    Scrub,
    PoolDrag { source: SourceId },
    ClipMove { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
}

enum TimelineHit {
    None,
    Lane { track: usize },
    ClipBody { track: usize, idx: usize, grab_t_offset: f64 },
    ClipTrimLeft { track: usize, idx: usize },
    ClipTrimRight { track: usize, idx: usize },
}

#[derive(Copy, Clone)]
struct TimelineLayout {
    top: f32,
    clips_x: f32,
    clips_w: f32,
    center_y: f32,
    lane_h: f32,
    duration: f64,
}

fn compute_lane_height(tracks_area_h: f32, n_tracks: usize) -> f32 {
    let n = n_tracks.max(1) as f32;
    let gaps = (n - 1.0).max(0.0) * TRACK_LANE_GAP;
    let avail = (tracks_area_h * TRACK_LANE_FILL - gaps).max(0.0);
    (avail / n)
        .clamp(TRACK_LANE_MIN_H, TRACK_LANE_MAX_H)
        .round()
}

impl TimelineLayout {
    fn cursor_to_t(&self, cursor_x: f32) -> f64 {
        let ratio = ((cursor_x - self.clips_x) / self.clips_w).clamp(0.0, 1.0) as f64;
        ratio * self.duration
    }

    fn t_to_x(&self, t: f64) -> f32 {
        self.clips_x + (t / self.duration) as f32 * self.clips_w
    }
}

/// Shorten `text` so it fits within `max_w` when rendered at `size_px`,
/// appending an ellipsis if truncation happened. Returns the original string
/// when it already fits, so the common case stays zero-allocation at the call
/// site (the caller passes a `&str` either way).
fn truncate_to_width(text: &TextRenderer, s: &str, size_px: f32, max_w: f32) -> String {
    if text.measure_width(s, size_px) <= max_w {
        return s.to_string();
    }
    let ellipsis = "…";
    let ell_w = text.measure_width(ellipsis, size_px);
    if ell_w > max_w {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0.0;
    for ch in s.chars() {
        let ch_w = text.measure_width(&ch.to_string(), size_px);
        if used + ch_w + ell_w > max_w {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push_str(ellipsis);
    out
}

fn format_timecode(t: f64) -> String {
    let total_ms = (t.max(0.0) * 1000.0) as u64;
    let ms = total_ms % 1000;
    let sec = total_ms / 1000;
    let m = sec / 60;
    let s = sec % 60;
    format!("{:02}:{:02}.{:03}", m, s, ms)
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

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    quads: QuadRenderer,
    text: TextRenderer,
    media: MediaPool,
    timeline: Timeline,
    audio: AudioEngine,
    cursor: [f32; 2],
    drag: DragMode,
    last_playing_source: Option<SourceId>,
}

impl State {
    async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
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

        // Start with V1, V2, A1, A2 — the model supports arbitrary mixes; this is
        // just a sensible default so the UI shows multiple lanes immediately.
        let mut timeline = Timeline::new();
        timeline.tracks.push(Track::new(TrackKind::Video));
        timeline.tracks.push(Track::new(TrackKind::Video));
        timeline.tracks.push(Track::new(TrackKind::Audio));
        timeline.tracks.push(Track::new(TrackKind::Audio));

        let mut media = MediaPool::new();
        for path in std::env::args().skip(1) {
            import_source(&mut media, &path, &device, &queue, &quads);
        }

        let state = State {
            instance,
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
            quads,
            text,
            media,
            timeline,
            audio: AudioEngine::new(),
            cursor: [0.0, 0.0],
            drag: DragMode::None,
            last_playing_source: None,
        };

        state.configure_surface();

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    fn configure_surface(&self) {
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

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        self.configure_surface();
    }

    fn timeline_top(&self) -> f32 {
        (self.size.height as f32 * TOP_BOTTOM_SPLIT).round()
    }

    fn timeline_layout(&self) -> TimelineLayout {
        let w = self.size.width as f32;
        let top = self.timeline_top();
        let bottom = self.size.height as f32;
        let tracks_top = top + TIMELINE_TOP_PAD;
        let tracks_area_h = (bottom - tracks_top).max(0.0);
        TimelineLayout {
            top,
            clips_x: TRACK_HEADER_WIDTH,
            clips_w: (w - TRACK_HEADER_WIDTH).max(1.0),
            center_y: (tracks_top + tracks_area_h * 0.5).round(),
            lane_h: compute_lane_height(tracks_area_h, self.timeline.tracks.len()),
            duration: self.timeline.duration().max(1.0),
        }
    }

    fn pool_hit(&self, cursor_x: f32, cursor_y: f32) -> Option<SourceId> {
        let w = self.size.width as f32;
        let media_w = (w * MEDIA_PREVIEW_SPLIT).round();
        let top_h = self.timeline_top();
        if cursor_x < 0.0 || cursor_x > media_w || cursor_y < POOL_LIST_TOP || cursor_y > top_h {
            return None;
        }
        let stride = POOL_ROW_HEIGHT + POOL_ROW_GAP;
        let rel_y = cursor_y - POOL_LIST_TOP;
        let i = (rel_y / stride).floor() as usize;
        let within = rel_y - i as f32 * stride;
        if within > POOL_ROW_HEIGHT {
            return None; // in the gap between rows
        }
        self.media.ids().get(i).copied()
    }

    /// Locate the visual track lane under `cursor_y`. Returns the track index
    /// whose lane *center* is nearest — gaps snap to the nearer lane so drops
    /// near a boundary feel forgiving.
    fn track_at_y(&self, cursor_y: f32, layout: &TimelineLayout) -> Option<usize> {
        if cursor_y < layout.top + TIMELINE_TOP_PAD {
            return None;
        }
        let stride = layout.lane_h + TRACK_LANE_GAP;
        let video_idxs: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video)
            .map(|(i, _)| i)
            .collect();
        let audio_idxs: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Audio)
            .map(|(i, _)| i)
            .collect();

        // Lanes start half_gap away from center_y on each side (the V/A
        // boundary's share of the inter-lane gap). Subtract it so the V1/A1
        // hitbox aligns with the rendered lane rect.
        let half_gap = TRACK_LANE_GAP * 0.5;
        if cursor_y < layout.center_y {
            let dy = (layout.center_y - cursor_y - half_gap).max(0.0);
            let visual_i = (dy / stride).floor() as usize;
            video_idxs.get(visual_i).copied()
        } else {
            let dy = (cursor_y - layout.center_y - half_gap).max(0.0);
            let visual_i = (dy / stride).floor() as usize;
            audio_idxs.get(visual_i).copied()
        }
    }

    fn timeline_hit(&self, cursor_x: f32, cursor_y: f32) -> TimelineHit {
        let layout = self.timeline_layout();
        if cursor_y < layout.top {
            return TimelineHit::None;
        }
        let Some(track_idx) = self.track_at_y(cursor_y, &layout) else {
            return TimelineHit::None;
        };
        if cursor_x < layout.clips_x {
            return TimelineHit::None;
        }
        let cursor_t = layout.cursor_to_t(cursor_x);
        let track = &self.timeline.tracks[track_idx];
        for (i, clip) in track.clips.iter().enumerate() {
            let x0 = layout.t_to_x(clip.timeline_start);
            let x1 = layout.t_to_x(clip.timeline_end());
            if cursor_x >= x0 - CLIP_EDGE_GRAB_PX && cursor_x <= x0 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimLeft { track: track_idx, idx: i };
            }
            if cursor_x >= x1 - CLIP_EDGE_GRAB_PX && cursor_x <= x1 + CLIP_EDGE_GRAB_PX {
                return TimelineHit::ClipTrimRight { track: track_idx, idx: i };
            }
            if cursor_x >= x0 && cursor_x <= x1 {
                return TimelineHit::ClipBody {
                    track: track_idx,
                    idx: i,
                    grab_t_offset: cursor_t - clip.timeline_start,
                };
            }
        }
        TimelineHit::Lane { track: track_idx }
    }

    fn begin_drag(&mut self) {
        let [cx, cy] = self.cursor;
        if let Some(source) = self.pool_hit(cx, cy) {
            self.drag = DragMode::PoolDrag { source };
            return;
        }
        match self.timeline_hit(cx, cy) {
            TimelineHit::ClipTrimLeft { track, idx } => {
                self.drag = DragMode::ClipTrimLeft { track, idx };
            }
            TimelineHit::ClipTrimRight { track, idx } => {
                self.drag = DragMode::ClipTrimRight { track, idx };
            }
            TimelineHit::ClipBody { track, idx, grab_t_offset } => {
                self.drag = DragMode::ClipMove { track, idx, grab_t_offset };
            }
            TimelineHit::Lane { .. } => {
                self.drag = DragMode::Scrub;
                self.apply_scrub();
            }
            TimelineHit::None => {}
        }
    }

    fn update_drag(&mut self) {
        match self.drag {
            DragMode::None | DragMode::PoolDrag { .. } => {}
            DragMode::Scrub => self.apply_scrub(),
            DragMode::ClipMove { track, idx, grab_t_offset } => {
                let layout = self.timeline_layout();
                // Allow vertical drag: if the cursor is over a different
                // same-kind track, relocate the clip there before nudging x.
                // Cross-kind moves (V↔A) are blocked — a video clip on an
                // audio lane has no meaningful playback behavior yet. Linked
                // siblings stay on their own tracks; only the dragged clip
                // changes track membership.
                let (track, idx) =
                    if let Some(hover) = self.track_at_y(self.cursor[1], &layout) {
                        let src_kind = self.timeline.tracks[track].kind;
                        let dst_kind = self.timeline.tracks[hover].kind;
                        if hover != track && src_kind == dst_kind {
                            let clip = self.timeline.tracks[track].clips.remove(idx);
                            let new_idx = self.timeline.tracks[hover].clips.len();
                            self.timeline.tracks[hover].clips.push(clip);
                            self.drag = DragMode::ClipMove {
                                track: hover,
                                idx: new_idx,
                                grab_t_offset,
                            };
                            (hover, new_idx)
                        } else {
                            (track, idx)
                        }
                    } else {
                        (track, idx)
                    };
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start =
                    self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = (cursor_t - grab_t_offset) - current_start;
                self.apply_move_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimLeft { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_start =
                    self.timeline.tracks[track].clips[idx].timeline_start;
                let desired_delta = cursor_t - current_start;
                self.apply_trim_left_delta(track, idx, desired_delta);
            }
            DragMode::ClipTrimRight { track, idx } => {
                let layout = self.timeline_layout();
                let cursor_t = layout.cursor_to_t(self.cursor[0]);
                let current_end =
                    self.timeline.tracks[track].clips[idx].timeline_end();
                let desired_delta = cursor_t - current_end;
                self.apply_trim_right_delta(track, idx, desired_delta);
            }
        }
    }

    /// Indices of every clip linked to `(track, idx)`, including itself.
    /// Unlinked clips return just their own position.
    fn linked_siblings(&self, track: usize, idx: usize) -> Vec<(usize, usize)> {
        let link = self.timeline.tracks[track].clips[idx].link;
        let Some(link_id) = link else {
            return vec![(track, idx)];
        };
        let mut v = Vec::new();
        for (ti, tr) in self.timeline.tracks.iter().enumerate() {
            for (ci, c) in tr.clips.iter().enumerate() {
                if c.link == Some(link_id) {
                    v.push((ti, ci));
                }
            }
        }
        v
    }

    fn apply_move_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        // Clamp so the earliest-starting sibling doesn't go negative —
        // applying the same delta everywhere preserves the sync offset.
        let min_start = siblings
            .iter()
            .map(|&(ti, ci)| self.timeline.tracks[ti].clips[ci].timeline_start)
            .fold(f64::INFINITY, f64::min);
        let delta = desired_delta.max(-min_start);
        for (ti, ci) in siblings {
            self.timeline.tracks[ti].clips[ci].timeline_start += delta;
        }
    }

    fn apply_trim_left_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        // Delta bounds: the same delta shifts every sibling's source_in and
        // timeline_start, so the allowed range is the intersection of each
        // sibling's own limits.
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            min_delta = min_delta.max(-c.source_in);
            min_delta = min_delta.max(-c.timeline_start);
            max_delta = max_delta.min(c.duration() - MIN_CLIP_DURATION);
        }
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_in += delta;
            c.timeline_start += delta;
        }
    }

    fn apply_trim_right_delta(&mut self, track: usize, idx: usize, desired_delta: f64) {
        let siblings = self.linked_siblings(track, idx);
        let mut min_delta = f64::NEG_INFINITY;
        let mut max_delta = f64::INFINITY;
        for &(ti, ci) in &siblings {
            let c = &self.timeline.tracks[ti].clips[ci];
            // Can't shrink below the minimum clip duration.
            min_delta = min_delta.max(MIN_CLIP_DURATION - c.duration());
            // Can't extend past the source's end — cap per-track since video
            // and audio streams of the same source can have different lengths.
            let src_dur = match self.timeline.tracks[ti].kind {
                TrackKind::Video => self.media.duration(c.source),
                TrackKind::Audio => self.media.audio_duration(c.source).unwrap_or(c.source_out),
            };
            max_delta = max_delta.min(src_dur - c.source_out);
        }
        let delta = desired_delta.clamp(min_delta, max_delta);
        for (ti, ci) in siblings {
            let c = &mut self.timeline.tracks[ti].clips[ci];
            c.source_out += delta;
        }
    }

    fn end_drag(&mut self) {
        if let DragMode::PoolDrag { source } = self.drag {
            let [cx, cy] = self.cursor;
            let layout = self.timeline_layout();
            if let Some(track_idx) = self.track_at_y(cy, &layout) {
                let drop_t = layout.cursor_to_t(cx).max(0.0);
                let kind = self.timeline.tracks[track_idx].kind;
                match kind {
                    TrackKind::Video => {
                        let dur = self.media.duration(source);
                        // Decide up front whether we're auto-pairing audio —
                        // only then do we need a link id, and both sides must
                        // use the same one.
                        let audio_target = self
                            .media
                            .has_audio(source)
                            .then(|| {
                                self.timeline
                                    .tracks
                                    .iter()
                                    .position(|t| t.kind == TrackKind::Audio)
                            })
                            .flatten();
                        let link = audio_target.map(|_| self.timeline.new_link_id());
                        self.timeline.tracks[track_idx].clips.push(Clip {
                            source,
                            source_in: 0.0,
                            source_out: dur,
                            timeline_start: drop_t,
                            link,
                        });
                        if let Some(audio_idx) = audio_target {
                            let adur = self.media.audio_duration(source).unwrap_or(dur);
                            self.timeline.tracks[audio_idx].clips.push(Clip {
                                source,
                                source_in: 0.0,
                                source_out: adur,
                                timeline_start: drop_t,
                                link,
                            });
                        }
                    }
                    TrackKind::Audio => {
                        if let Some(adur) = self.media.audio_duration(source) {
                            self.timeline.tracks[track_idx].clips.push(Clip {
                                source,
                                source_in: 0.0,
                                source_out: adur,
                                timeline_start: drop_t,
                                link: None,
                            });
                        }
                        // Dropping a video-only source on an audio lane is a
                        // no-op — there's nothing to play back there.
                    }
                }
            }
        }
        self.drag = DragMode::None;
    }

    fn apply_scrub(&mut self) {
        let duration = self.timeline.duration();
        if duration <= 0.0 {
            return;
        }
        let layout = self.timeline_layout();
        let t = layout.cursor_to_t(self.cursor[0]);
        // Audio engine owns the playhead — setting position also flushes any
        // pre-mixed samples so the next tick refills from the new time,
        // keeping scrub snappy instead of dragging 150ms of stale audio.
        self.audio.set_position(t);
    }

    fn current_fps(&self) -> f64 {
        let t = self.audio.position();
        self.timeline
            .topmost_video_clip(t)
            .and_then(|(_, c)| self.media.get(c.source))
            .map(|s| s.stream.frame_rate())
            .unwrap_or(30.0)
    }

    fn step_frame(&mut self, dir: f64) {
        if self.audio.playing() {
            self.audio.set_playing(false);
        }
        let fps = self.current_fps().max(1.0);
        let dt = dir / fps;
        let mut new_t = (self.audio.position() + dt).max(0.0);
        let duration = self.timeline.duration();
        if duration > 0.0 {
            new_t = new_t.min(duration);
        }
        self.audio.set_position(new_t);
    }

    fn split_at_playhead(&mut self) {
        let t = self.audio.position();
        self.timeline.split_at(t);
    }

    fn toggle_playback(&mut self) {
        self.audio.toggle();
    }

    fn import_file(&mut self, path: &str) {
        import_source(
            &mut self.media,
            path,
            &self.device,
            &self.queue,
            &self.quads,
        );
    }

    fn open_file_picker(&mut self) {
        // Blocking dialog is fine here: a single-user editor pausing the event
        // loop while the OS picker is up is the expected behavior.
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("video", &["mp4", "mov", "mkv", "webm", "avi", "m4v"])
            .pick_files()
        else {
            return;
        };
        for path in paths {
            if let Some(p) = path.to_str() {
                self.import_file(p);
            }
        }
    }

    fn render(&mut self) {
        let surface_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture) => texture,
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => return,
            wgpu::CurrentSurfaceTexture::Suboptimal(_) | wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure_surface();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                unreachable!("No error scope registered, so validation errors will panic")
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.configure_surface();
                return;
            }
        };
        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.surface_format.add_srgb_suffix()),
                ..Default::default()
            });

        let w = self.size.width as f32;
        let h = self.size.height as f32;
        let top_h = (h * TOP_BOTTOM_SPLIT).round();
        let bottom_h = h - top_h;
        let media_w = (w * MEDIA_PREVIEW_SPLIT).round();
        let preview_w = w - media_w;

        // Clamp playhead to [0, duration]. The audio engine drives time forward
        // while playing; if we ran past the end, pause and park at the end so
        // video and audio agree on "stopped".
        let duration = self.timeline.duration();
        let mut t = self.audio.position();
        if duration <= 0.0 {
            self.audio.set_playing(false);
            self.audio.set_position(0.0);
            t = 0.0;
        } else if t >= duration {
            self.audio.set_playing(false);
            self.audio.set_position(duration);
            t = duration;
        }

        // Refill the audio mix buffer before doing render work. Done early so
        // if render is slow the audio thread still has samples queued.
        {
            let Self {
                audio,
                timeline,
                media,
                ..
            } = self;
            audio.tick(timeline, media);
        }

        self.quads.clear();
        self.quads
            .push(Quad::colored([0.0, 0.0], [media_w, top_h], MEDIA_POOL_COLOR));
        self.quads.push(Quad::colored(
            [media_w, 0.0],
            [preview_w, top_h],
            PREVIEW_COLOR,
        ));

        // --- Preview: topmost active video clip ---
        // Scoped disjoint borrows so the decoder advance + textured-quad push can
        // share this block without leaking borrows past it.
        {
            let Self {
                media,
                timeline,
                quads,
                queue,
                last_playing_source,
                ..
            } = self;

            let active_info = timeline
                .topmost_video_clip(t)
                .map(|(_, c)| (c.source, c.source_time(t)));
            if let Some((source_id, source_t)) = active_info {
                *last_playing_source = Some(source_id);
                if let Some(src) = media.get_mut(source_id) {
                    src.stream.goto(queue, source_t);

                    let vw = src.stream.width() as f32;
                    let vh = src.stream.height() as f32;
                    let scale = (preview_w / vw).min(top_h / vh);
                    let draw_w = vw * scale;
                    let draw_h = vh * scale;
                    let dx = media_w + (preview_w - draw_w) * 0.5;
                    let dy = (top_h - draw_h) * 0.5;
                    quads.push_with(
                        Quad::textured([dx, dy], [draw_w, draw_h]),
                        Some(src.stream.texture()),
                    );
                }
            } else {
                *last_playing_source = None;
            }
        }

        // --- Timeline panel background ---
        self.quads
            .push(Quad::colored([0.0, top_h], [w, bottom_h], TIMELINE_COLOR));

        // --- Timeline tracks ---
        let tracks_top = top_h + TIMELINE_TOP_PAD;
        let tracks_bottom = h;
        let tracks_area_h = (tracks_bottom - tracks_top).max(0.0);
        // Snap center to a whole pixel so derived lane_y values don't land on
        // half-pixels (which renders as a blurry edge under bilinear sampling).
        let center_y = (tracks_top + tracks_area_h * 0.5).round();
        let half_gap = TRACK_LANE_GAP * 0.5;
        let lane_h = compute_lane_height(tracks_area_h, self.timeline.tracks.len());

        let video_tracks: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Video)
            .map(|(i, _)| i)
            .collect();
        let audio_tracks: Vec<usize> = self
            .timeline
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, tr)| tr.kind == TrackKind::Audio)
            .map(|(i, _)| i)
            .collect();

        // Divider between video (above) and audio (below) regions.
        self.quads.push(Quad::colored(
            [0.0, center_y - 0.5],
            [w, 1.0],
            DIVIDER_COLOR,
        ));

        // Use the real duration so clip widths, playhead, and scrub all share the
        // same denominator. Fold in any currently-dragged clip's duration so the
        // ghost previews at the same scale it'll occupy after drop, instead of
        // ballooning to screen width when the timeline is empty (timeline=0 →
        // `.max(1.0)` → ghost_w = clip_dur * clips_w).
        let ghost_dur = if let DragMode::PoolDrag { source } = self.drag {
            self.media.duration(source)
        } else {
            0.0
        };
        let timeline_duration_display = self.timeline.duration().max(ghost_dur).max(1.0);
        let clips_x = TRACK_HEADER_WIDTH;
        let clips_w = (w - TRACK_HEADER_WIDTH).max(1.0);

        // V1 sits just above the divider (leaving half_gap between its bottom
        // and center_y), V2 stacks above V1 with a full TRACK_LANE_GAP between.
        for (visual_i, &track_idx) in video_tracks.iter().enumerate() {
            let lane_y = center_y
                - half_gap
                - lane_h
                - visual_i as f32 * (lane_h + TRACK_LANE_GAP);
            self.draw_track_lane(
                lane_y,
                lane_h,
                clips_x,
                clips_w,
                timeline_duration_display,
                track_idx,
                visual_i,
            );
        }
        // A1 sits just below the divider, A2 below A1, etc.
        for (visual_i, &track_idx) in audio_tracks.iter().enumerate() {
            let lane_y = center_y + half_gap + visual_i as f32 * (lane_h + TRACK_LANE_GAP);
            self.draw_track_lane(
                lane_y,
                lane_h,
                clips_x,
                clips_w,
                timeline_duration_display,
                track_idx,
                visual_i,
            );
        }

        // --- Playhead: drawn last so it's on top of clips ---
        if self.timeline.duration() > 0.0 {
            let ratio = (t / self.timeline.duration()).clamp(0.0, 1.0) as f32;
            let px = (clips_x + ratio * clips_w - PLAYHEAD_WIDTH * 0.5).round();
            self.quads.push(Quad::colored(
                [px, top_h],
                [PLAYHEAD_WIDTH, bottom_h],
                PLAYHEAD_COLOR,
            ));
        }

        // --- Pool-drag ghost: previews where the clip will land ---
        // Start-aligned to the cursor (matches `end_drag`'s drop_t semantics)
        // and snapped to the hovered lane's y when over one, so the preview
        // rect is exactly the rect that'll be created on mouse-up.
        if let DragMode::PoolDrag { source } = self.drag {
            let dur = self.media.duration(source);
            let ghost_w = ((dur / timeline_duration_display) as f32 * clips_w).max(40.0);
            let ghost_h = lane_h;
            let layout = self.timeline_layout();
            let over_lane = self.track_at_y(self.cursor[1], &layout);
            let gx = self.cursor[0].max(clips_x);
            let (gy, ghost_color) = match over_lane {
                Some(track_idx) => match self.timeline.tracks[track_idx].kind {
                    TrackKind::Video => {
                        let visual_i =
                            video_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        let y = center_y
                            - half_gap
                            - lane_h
                            - visual_i as f32 * (lane_h + TRACK_LANE_GAP);
                        (y, DRAG_GHOST_VIDEO_COLOR)
                    }
                    TrackKind::Audio => {
                        let visual_i =
                            audio_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        let y = center_y + half_gap + visual_i as f32 * (lane_h + TRACK_LANE_GAP);
                        (y, DRAG_GHOST_AUDIO_COLOR)
                    }
                },
                None => (self.cursor[1] - ghost_h * 0.5, DRAG_GHOST_VIDEO_COLOR),
            };
            self.quads
                .push(Quad::colored([gx, gy], [ghost_w, ghost_h], ghost_color));
        }

        // --- Media pool list ---
        self.draw_media_pool_list(media_w, top_h);

        // --- Panel labels ---
        let baseline_y = LABEL_PAD + self.text.ascent(LABEL_SIZE);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, baseline_y],
            "MEDIA POOL",
            LABEL_SIZE,
            LABEL_COLOR,
        );
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [media_w + LABEL_PAD, baseline_y],
            "PREVIEW",
            LABEL_SIZE,
            LABEL_COLOR,
        );
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [LABEL_PAD, top_h + LABEL_PAD + self.text.ascent(LABEL_SIZE)],
            "TIMELINE",
            LABEL_SIZE,
            LABEL_COLOR,
        );

        // --- Playback timer: bottom-right of preview, above any video frame ---
        let timer_text = format!(
            "{} / {}",
            format_timecode(t),
            format_timecode(self.timeline.duration())
        );
        let timer_w = self.text.measure_width(&timer_text, TIMER_SIZE);
        let timer_ascent = self.text.ascent(TIMER_SIZE);
        let timer_baseline = top_h - TIMER_PAD;
        let timer_left = w - TIMER_PAD - timer_w;
        let bg_pad = 6.0;
        self.quads.push(Quad::colored(
            [timer_left - bg_pad, timer_baseline - timer_ascent - bg_pad * 0.5],
            [timer_w + bg_pad * 2.0, timer_ascent + bg_pad],
            TIMER_BG_COLOR,
        ));
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [timer_left, timer_baseline],
            &timer_text,
            TIMER_SIZE,
            TIMER_COLOR,
        );

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            self.quads
                .draw(&self.device, &self.queue, &mut pass, [w, h]);
        }

        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();
    }

    fn draw_media_pool_list(&mut self, pool_w: f32, pool_h: f32) {
        let row_x = LABEL_PAD;
        let row_w = (pool_w - LABEL_PAD * 2.0).max(1.0);

        for (i, &id) in self.media.ids().iter().enumerate() {
            let row_y = POOL_LIST_TOP + i as f32 * (POOL_ROW_HEIGHT + POOL_ROW_GAP);
            if row_y + POOL_ROW_HEIGHT > pool_h {
                break; // beyond panel; scrolling will come later
            }
            let Some(src) = self.media.get(id) else {
                continue;
            };

            self.quads.push(Quad::colored(
                [row_x, row_y],
                [row_w, POOL_ROW_HEIGHT],
                POOL_ROW_COLOR,
            ));

            // Thumbnail slot (dark background so letterboxed thumbs look intentional).
            let slot_x = row_x + POOL_ROW_PAD;
            let slot_y = row_y + POOL_ROW_PAD;
            self.quads.push(Quad::colored(
                [slot_x, slot_y],
                [POOL_THUMB_W, POOL_THUMB_H],
                POOL_THUMB_BG,
            ));

            // Fit the baked thumbnail into the slot, preserving source aspect.
            let thumb = src.stream.thumbnail();
            let tw = thumb.width as f32;
            let th = thumb.height as f32;
            let scale = (POOL_THUMB_W / tw).min(POOL_THUMB_H / th);
            let dw = (tw * scale).round();
            let dh = (th * scale).round();
            let dx = (slot_x + (POOL_THUMB_W - dw) * 0.5).round();
            let dy = (slot_y + (POOL_THUMB_H - dh) * 0.5).round();
            self.quads
                .push_with(Quad::textured([dx, dy], [dw, dh]), Some(thumb));

            // Duration pill in the bottom-right of the thumb slot.
            let dur_text = format_timecode(src.stream.duration());
            let dur_w = self.text.measure_width(&dur_text, POOL_ITEM_META_SIZE);
            let dur_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
            let pill_pad_x = 4.0;
            let pill_pad_y = 2.0;
            let pill_w = dur_w + pill_pad_x * 2.0;
            let pill_h = dur_ascent + pill_pad_y * 2.0;
            let pill_inset = 3.0;
            let pill_x = slot_x + POOL_THUMB_W - pill_inset - pill_w;
            let pill_y = slot_y + POOL_THUMB_H - pill_inset - pill_h;
            self.quads
                .push(Quad::colored([pill_x, pill_y], [pill_w, pill_h], POOL_DUR_BG));
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [pill_x + pill_pad_x, pill_y + pill_pad_y + dur_ascent],
                &dur_text,
                POOL_ITEM_META_SIZE,
                POOL_DUR_TEXT,
            );

            // Name to the right of the thumb, vertically centered. Clamp with
            // an ellipsis if the filename would otherwise bleed into the preview.
            let name_x = slot_x + POOL_THUMB_W + POOL_ROW_PAD + 4.0;
            let name_max_w = (row_x + row_w - POOL_ROW_PAD - name_x).max(0.0);
            let name_ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
            let name_baseline = row_y + (POOL_ROW_HEIGHT + name_ascent) * 0.5;
            let name = truncate_to_width(&self.text, &src.name, POOL_ITEM_NAME_SIZE, name_max_w);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [name_x, name_baseline],
                &name,
                POOL_ITEM_NAME_SIZE,
                CLIP_LABEL_COLOR,
            );
        }
    }

    fn draw_track_lane(
        &mut self,
        lane_y: f32,
        lane_h: f32,
        clips_x: f32,
        clips_w: f32,
        timeline_duration: f64,
        track_idx: usize,
        visual_i: usize,
    ) {
        let track = &self.timeline.tracks[track_idx];
        let (clip_color, label_prefix) = match track.kind {
            TrackKind::Video => (VIDEO_CLIP_COLOR, "V"),
            TrackKind::Audio => (AUDIO_CLIP_COLOR, "A"),
        };

        // Lane background.
        self.quads.push(Quad::colored(
            [0.0, lane_y],
            [clips_x + clips_w, lane_h],
            LANE_COLOR,
        ));

        // Track header label (V1, V2, A1, ...).
        let header = format!("{}{}", label_prefix, visual_i + 1);
        let baseline = lane_y + (lane_h + self.text.ascent(CLIP_LABEL_SIZE)) * 0.5;
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [8.0, baseline],
            &header,
            CLIP_LABEL_SIZE,
            TRACK_LABEL_COLOR,
        );

        // Clips.
        for clip in &track.clips {
            let x = clips_x + (clip.timeline_start / timeline_duration) as f32 * clips_w;
            let cw = ((clip.duration() / timeline_duration) as f32 * clips_w).max(1.0);
            self.quads
                .push(Quad::colored([x, lane_y], [cw, lane_h], clip_color));

            // Waveform bars for audio clips. One 1px-wide vertical rect per
            // pixel column, height proportional to the max peak in that
            // column's source-time window. Label is drawn after so it sits
            // on top of the waveform.
            if track.kind == TrackKind::Audio && cw > 1.0 {
                if let Some(src) = self.media.get(clip.source) {
                    if let Some(wf) = src.waveform.as_ref() {
                        if !wf.peaks.is_empty() {
                            let clip_dur = clip.duration();
                            let seconds_per_px = clip_dur / cw as f64;
                            let mid_y = lane_y + lane_h * 0.5;
                            let max_half_h = (lane_h * 0.45_f32).max(1.0);
                            let n_cols = cw.ceil() as i32;
                            let n_peaks = wf.peaks.len();
                            for col in 0..n_cols {
                                let src_t_start =
                                    clip.source_in + col as f64 * seconds_per_px;
                                let src_t_end = src_t_start + seconds_per_px;
                                let idx_start =
                                    (src_t_start / wf.bucket_seconds) as usize;
                                let mut idx_end = ((src_t_end / wf.bucket_seconds)
                                    .ceil()
                                    as usize)
                                    .max(idx_start + 1);
                                if idx_start >= n_peaks {
                                    break;
                                }
                                idx_end = idx_end.min(n_peaks);
                                if idx_start >= idx_end {
                                    continue;
                                }
                                let mut peak = 0.0f32;
                                for &p in &wf.peaks[idx_start..idx_end] {
                                    if p > peak {
                                        peak = p;
                                    }
                                }
                                let half_h = (peak * max_half_h).max(0.5);
                                let px = x + col as f32;
                                self.quads.push(Quad::colored(
                                    [px, mid_y - half_h],
                                    [1.0, half_h * 2.0],
                                    AUDIO_WAVE_COLOR,
                                ));
                            }
                        }
                    }
                }
            }

            if let Some(src) = self.media.get(clip.source) {
                let label_pad = 6.0;
                let label_baseline = lane_y + self.text.ascent(CLIP_LABEL_SIZE) + 4.0;
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [x + label_pad, label_baseline],
                    &src.name,
                    CLIP_LABEL_SIZE,
                    CLIP_LABEL_COLOR,
                );
            }
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("Ruve")
                        .with_inner_size(LogicalSize::new(1920.0, 1080.0)),
                )
                .unwrap(),
        );

        let state = pollster::block_on(State::new(
            event_loop.owned_display_handle(),
            window.clone(),
        ));
        self.state = Some(state);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = self.state.as_mut().unwrap();
        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::DroppedFile(path) => {
                if let Some(path_str) = path.to_str() {
                    state.import_file(path_str);
                }
            }
            WindowEvent::RedrawRequested => {
                state.render();
                state.get_window().request_redraw();
            }
            WindowEvent::Resized(size) => {
                state.resize(size);
            }
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor = [position.x as f32, position.y as f32];
                state.update_drag();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                state.begin_drag();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                state.end_drag();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        repeat,
                        ..
                    },
                ..
            } => match code {
                // Arrows repeat so holding steps through frames.
                KeyCode::ArrowLeft => state.step_frame(-1.0),
                KeyCode::ArrowRight => state.step_frame(1.0),
                // The rest are edge-triggered to avoid repeat spam.
                _ if repeat => {}
                KeyCode::Space => state.toggle_playback(),
                KeyCode::KeyO => state.open_file_picker(),
                KeyCode::KeyS => state.split_at_playhead(),
                _ => {}
            },
            _ => (),
        }
    }
}

fn main() {
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}
