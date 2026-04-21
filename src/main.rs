mod quad;
mod text;
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

use quad::{Quad, QuadRenderer};
use text::TextRenderer;
use video::VideoStream;

// Layout split ratios — tweak to taste.
const TOP_BOTTOM_SPLIT: f32 = 0.55; // top section takes this fraction of height
const MEDIA_PREVIEW_SPLIT: f32 = 0.28; // media pool takes this fraction of top-section width

// Panel colors (sRGB).
const MEDIA_POOL_COLOR: [f32; 4] = [0.14, 0.14, 0.16, 1.0];
const PREVIEW_COLOR: [f32; 4] = [0.04, 0.04, 0.05, 1.0];
const TIMELINE_COLOR: [f32; 4] = [0.10, 0.10, 0.12, 1.0];
const LABEL_COLOR: [f32; 4] = [0.65, 0.65, 0.70, 1.0];
const LABEL_SIZE: f32 = 13.0;
const LABEL_PAD: f32 = 10.0;
const PLAYHEAD_COLOR: [f32; 4] = [0.95, 0.35, 0.35, 1.0];
const PLAYHEAD_WIDTH: f32 = 2.0;

struct Clock {
    playing: bool,
    anchor_pos: f64,
    anchor_instant: Instant,
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
    video: Option<VideoStream>,
    clock: Clock,
    cursor: [f32; 2],
    scrubbing: bool,
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

        let video = std::env::args()
            .nth(1)
            .and_then(|path| match VideoStream::open(&path, &device, &queue, &quads) {
                Ok(stream) => Some(stream),
                Err(e) => {
                    log::error!("failed to load video {path}: {e}");
                    None
                }
            });

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
            video,
            clock: Clock::new(),
            cursor: [0.0, 0.0],
            scrubbing: false,
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
        let Some(v) = self.video.as_mut() else {
            return;
        };
        let duration = v.duration();
        if duration <= 0.0 {
            return;
        }
        let w = self.size.width as f32;
        let ratio = (self.cursor[0] / w).clamp(0.0, 1.0) as f64;
        let t = ratio * duration;
        v.seek(t);
        self.clock.set_pos(t);
    }

    fn toggle_playback(&mut self) {
        self.clock.toggle();
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

        self.quads.clear();
        self.quads
            .push(Quad::colored([0.0, 0.0], [media_w, top_h], MEDIA_POOL_COLOR));
        self.quads.push(Quad::colored(
            [media_w, 0.0],
            [preview_w, top_h],
            PREVIEW_COLOR,
        ));
        let mut playhead_ratio: Option<f32> = None;
        if let Some(v) = self.video.as_mut() {
            let pos = self.clock.pos();
            v.advance_to(&self.queue, pos);
            let vw = v.width() as f32;
            let vh = v.height() as f32;
            let scale = (preview_w / vw).min(top_h / vh);
            let draw_w = vw * scale;
            let draw_h = vh * scale;
            let dx = media_w + (preview_w - draw_w) * 0.5;
            let dy = (top_h - draw_h) * 0.5;
            self.quads
                .push_with(Quad::textured([dx, dy], [draw_w, draw_h]), Some(v.texture()));

            let duration = v.duration();
            if duration > 0.0 {
                playhead_ratio = Some((pos / duration).clamp(0.0, 1.0) as f32);
            }
        }
        self.quads
            .push(Quad::colored([0.0, top_h], [w, bottom_h], TIMELINE_COLOR));

        if let Some(ratio) = playhead_ratio {
            let px = (ratio * w - PLAYHEAD_WIDTH * 0.5).round();
            self.quads.push(Quad::colored(
                [px, top_h],
                [PLAYHEAD_WIDTH, bottom_h],
                PLAYHEAD_COLOR,
            ));
        }

        // Panel labels (baseline y = top + pad + ascent).
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
