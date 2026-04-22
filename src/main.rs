mod media;
mod quad;
mod text;
mod timeline;
mod video;

use std::sync::Arc;
use std::time::Instant;

use winit::{
    application::ApplicationHandler,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

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
const TRACK_LANE_HEIGHT: f32 = 32.0;
const TRACK_LANE_GAP: f32 = 2.0;
const TRACK_HEADER_WIDTH: f32 = 48.0;
const TIMELINE_TOP_PAD: f32 = 30.0; // clear space for the "TIMELINE" label

// Media pool list layout.
const POOL_LIST_TOP: f32 = 36.0; // below the MEDIA POOL label
const POOL_ROW_HEIGHT: f32 = 36.0;
const POOL_ROW_GAP: f32 = 4.0;
const POOL_ROW_PAD: f32 = 8.0;
const POOL_ROW_COLOR: [f32; 4] = [0.20, 0.20, 0.24, 1.0];
const POOL_ROW_ACCENT: [f32; 4] = [0.30, 0.45, 0.70, 1.0];
const POOL_ITEM_NAME_SIZE: f32 = 12.0;
const POOL_ITEM_META_SIZE: f32 = 10.0;
const POOL_META_COLOR: [f32; 4] = [0.55, 0.55, 0.60, 1.0];

struct Clock {
    playing: bool,
    anchor_pos: f64,
    anchor_instant: Instant,
}

fn format_timecode(t: f64) -> String {
    let total_ms = (t.max(0.0) * 1000.0) as u64;
    let ms = total_ms % 1000;
    let sec = total_ms / 1000;
    let m = sec / 60;
    let s = sec % 60;
    format!("{:02}:{:02}.{:03}", m, s, ms)
}

impl Clock {
    fn new() -> Self {
        Self {
            playing: true,
            anchor_pos: 0.0,
            anchor_instant: Instant::now(),
        }
    }

    fn pos(&self) -> f64 {
        if self.playing {
            self.anchor_pos + self.anchor_instant.elapsed().as_secs_f64()
        } else {
            self.anchor_pos
        }
    }

    fn toggle(&mut self) {
        if self.playing {
            self.anchor_pos = self.pos();
            self.playing = false;
        } else {
            self.anchor_instant = Instant::now();
            self.playing = true;
        }
    }

    fn set_pos(&mut self, t: f64) {
        self.anchor_pos = t.max(0.0);
        self.anchor_instant = Instant::now();
    }

    fn pause_at(&mut self, t: f64) {
        self.anchor_pos = t.max(0.0);
        self.playing = false;
    }
}

/// Import a file into the pool. If it loaded, append a clip onto V1 after the
/// last existing clip — this keeps dropped files immediately playable until we
/// build a pool-to-timeline drag interaction.
fn import_and_append(
    media: &mut MediaPool,
    timeline: &mut Timeline,
    path: &str,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    quads: &QuadRenderer,
) {
    match media.add(path, device, queue, quads) {
        Ok(id) => {
            let dur = media.duration(id);
            let v1 = &mut timeline.tracks[0];
            let start = v1.clips.last().map_or(0.0, |c| c.timeline_end());
            v1.clips.push(Clip {
                source: id,
                source_in: 0.0,
                source_out: dur,
                timeline_start: start,
            });
        }
        Err(e) => log::error!("failed to load {path}: {e}"),
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
    clock: Clock,
    cursor: [f32; 2],
    scrubbing: bool,
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
            import_and_append(&mut media, &mut timeline, &path, &device, &queue, &quads);
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
            clock: Clock::new(),
            cursor: [0.0, 0.0],
            scrubbing: false,
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

    fn seek_to_cursor_x(&mut self) {
        let duration = self.timeline.duration();
        if duration <= 0.0 {
            return;
        }
        let w = self.size.width as f32;
        let clips_w = (w - TRACK_HEADER_WIDTH).max(1.0);
        let ratio = ((self.cursor[0] - TRACK_HEADER_WIDTH) / clips_w).clamp(0.0, 1.0) as f64;
        let t = ratio * duration;
        // Only update the clock — render() will do the decoder work at most once
        // per frame. Doing the seek here too would pile up expensive ffmpeg seeks
        // on every mousemove and freeze the event loop.
        self.clock.set_pos(t);
    }

    fn toggle_playback(&mut self) {
        self.clock.toggle();
    }

    fn import_file(&mut self, path: &str) {
        import_and_append(
            &mut self.media,
            &mut self.timeline,
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

        // Tick the clock. Pause at end of timeline so we don't run past the content.
        let duration = self.timeline.duration();
        let mut t = self.clock.pos();
        if duration > 0.0 && t >= duration {
            self.clock.pause_at(duration);
            t = duration;
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
        let center_y = tracks_top + tracks_area_h * 0.5;

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
        // same denominator. Empty timelines skip the clip loop entirely, so the
        // `.max(1.0)` guard is only there to avoid NaN from a 0/0 division.
        let timeline_duration_display = self.timeline.duration().max(1.0);
        let clips_x = TRACK_HEADER_WIDTH;
        let clips_w = (w - TRACK_HEADER_WIDTH).max(1.0);

        // V1 sits just above the divider, V2 above V1, etc.
        for (visual_i, &track_idx) in video_tracks.iter().enumerate() {
            let lane_y = center_y - (visual_i as f32 + 1.0) * (TRACK_LANE_HEIGHT + TRACK_LANE_GAP)
                + TRACK_LANE_GAP;
            self.draw_track_lane(
                lane_y,
                clips_x,
                clips_w,
                timeline_duration_display,
                track_idx,
                visual_i,
            );
        }
        // A1 sits just below the divider, A2 below A1, etc.
        for (visual_i, &track_idx) in audio_tracks.iter().enumerate() {
            let lane_y = center_y + visual_i as f32 * (TRACK_LANE_HEIGHT + TRACK_LANE_GAP);
            self.draw_track_lane(
                lane_y,
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
            // Left-edge accent strip so video/audio is glanceable later.
            self.quads.push(Quad::colored(
                [row_x, row_y],
                [3.0, POOL_ROW_HEIGHT],
                POOL_ROW_ACCENT,
            ));

            let name_baseline = row_y + POOL_ROW_PAD + self.text.ascent(POOL_ITEM_NAME_SIZE);
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [row_x + POOL_ROW_PAD + 4.0, name_baseline],
                &src.name,
                POOL_ITEM_NAME_SIZE,
                CLIP_LABEL_COLOR,
            );

            let meta = format_timecode(src.stream.duration());
            let meta_baseline =
                row_y + POOL_ROW_HEIGHT - POOL_ROW_PAD * 0.5;
            self.text.draw(
                &self.queue,
                &mut self.quads,
                [row_x + POOL_ROW_PAD + 4.0, meta_baseline],
                &meta,
                POOL_ITEM_META_SIZE,
                POOL_META_COLOR,
            );
        }
    }

    fn draw_track_lane(
        &mut self,
        lane_y: f32,
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
            [clips_x + clips_w, TRACK_LANE_HEIGHT],
            LANE_COLOR,
        ));

        // Track header label (V1, V2, A1, ...).
        let header = format!("{}{}", label_prefix, visual_i + 1);
        let baseline = lane_y + (TRACK_LANE_HEIGHT + self.text.ascent(CLIP_LABEL_SIZE)) * 0.5;
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
                .push(Quad::colored([x, lane_y], [cw, TRACK_LANE_HEIGHT], clip_color));

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
                .create_window(Window::default_attributes())
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
                if state.scrubbing {
                    state.seek_to_cursor_x();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if state.cursor[1] >= state.timeline_top() {
                    state.scrubbing = true;
                    state.seek_to_cursor_x();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                state.scrubbing = false;
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(KeyCode::Space),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => {
                state.toggle_playback();
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(KeyCode::KeyO),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => {
                state.open_file_picker();
            }
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
