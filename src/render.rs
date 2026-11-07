//! One frame, start to finish.
//!
//! [`State::render`] is the only place the drawing order is decided. Quads are
//! painted back to front with no depth buffer, so the sequence of calls here
//! *is* the layering: panels, then their contents, then the chrome on top, then
//! the splitter handle and the popup over everything.

use crate::canvas::Canvas;
use crate::compose::Layer;
use crate::fmt::{fmt_fps, format_timecode, truncate_to_width};
use crate::input::DragMode;
use crate::layout::{
    pool_row_close_rect, transform_handle_rects, PoolCell, PoolTab, PoolView, Splitter,
};
use crate::audio::Waveform;
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::theme::*;
use crate::timeline::SourceId;
use crate::ui::Rect;

/// Draw a whole source's waveform into a box, one column per pixel of width.
///
/// The media pool's stand-in for a thumbnail on a source with no picture. Same
/// column-per-pixel construction as the timeline's waveform, but summarizing
/// the entire file rather than a clip's slice of it, so the shape in the pool
/// is the shape of the file.
fn draw_waveform_summary(
    quads: &mut QuadRenderer,
    wf: &Waveform,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    if wf.peaks.is_empty() || w < 1.0 {
        return;
    }
    let mid_y = y + h * 0.5;
    let max_half_h = (h * 0.42).max(1.0);
    let cols = w as i32;
    for col in 0..cols {
        let start = wf.peaks.len() * col as usize / cols as usize;
        let end = (wf.peaks.len() * (col + 1) as usize / cols as usize).max(start + 1);
        let peak = wf.peaks[start..end.min(wf.peaks.len())]
            .iter()
            .copied()
            .fold(0.0_f32, f32::max);
        let half_h = (peak * max_half_h).max(0.5);
        quads.push(Quad::colored(
            [x + col as f32, mid_y - half_h],
            [1.0, half_h * 2.0],
            AUDIO_WAVE_COLOR,
        ));
    }
}

/// One composed layer as a textured quad in the preview panel.
///
/// Two jobs at once, because they are the same arithmetic: mapping canvas
/// pixels into panel points, and trimming a layer that hangs off the canvas.
/// The trim is done here rather than by a scissor rect because the quad
/// renderer has no vertical clip — and because cutting the uv by the same
/// fraction as the rect is exact for an axis-aligned quad, so a layer half off
/// the edge shows half its picture rather than all of it squeezed.
///
/// `None` when the layer is entirely off canvas.
fn canvas_quad(
    layer: &Layer,
    canvas: Canvas,
    canvas_x: f32,
    canvas_y: f32,
    canvas_scale: f32,
) -> Option<Quad> {
    let [x, y, w, h] = layer.rect;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let [vx, vy, vw, vh] = layer.visible_rect(canvas.width, canvas.height)?;
    let mut quad = Quad::textured(
        [canvas_x + vx * canvas_scale, canvas_y + vy * canvas_scale],
        [vw * canvas_scale, vh * canvas_scale],
    );
    quad.uv = [
        (vx - x) / w,
        (vy - y) / h,
        (vx + vw - x) / w,
        (vy + vh - y) / h,
    ];
    // White, so the texture passes through untouched, and the alpha channel
    // carries the layer's opacity — the blend the pipeline already does for
    // every other translucent thing on screen.
    quad.color = [1.0, 1.0, 1.0, layer.alpha];
    Some(quad)
}

impl State {
    pub(crate) fn render(&mut self) {
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

        // The export worker reports through a mutex, not the event loop, so
        // this poll is what turns a finished render into a status message.
        self.poll_export();

        self.update_cursor_icon();

        let [w, h] = self.logical_size();
        let top_h = self.timeline_top();
        let media_w = self.media_pool_w();
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
            let audio_tracks = self.audio_tracks();
            let Self { audio, media, .. } = self;
            audio.tick(&audio_tracks, media);
        }

        let preview_h = (top_h - TRANSPORT_BAR_H).max(0.0);

        self.quads.clear();
        self.quads
            .push(Quad::colored([0.0, 0.0], [media_w, top_h], MEDIA_POOL_COLOR));
        self.quads.push(Quad::colored(
            [media_w, 0.0],
            [preview_w, preview_h],
            PREVIEW_COLOR,
        ));
        self.quads.push(Quad::colored(
            [media_w, preview_h],
            [preview_w, TRANSPORT_BAR_H],
            TRANSPORT_BAR_COLOR,
        ));
        // Panel edges. Drawn just inside the lighter panel so the darker well
        // beside it stays a clean field.
        self.quads.push(Quad::colored(
            [media_w - 1.0, 0.0],
            [1.0, top_h],
            PANEL_BORDER_COLOR,
        ));
        self.quads.push(Quad::colored(
            [media_w, preview_h],
            [preview_w, 1.0],
            PANEL_BORDER_COLOR,
        ));

        let canvas = self.draw_preview(media_w, preview_w, preview_h, t);
        // Before the timeline is drawn, so a page forward lands in this frame
        // rather than leaving the playhead off the edge for one.
        self.follow_playhead(t);
        self.draw_timeline_panel(w, h, top_h, t);
        {
            let Self { media, device, queue, quads, .. } = self;
            media.poll_filmstrips(device, queue, quads);
            media.release_idle_frames();
        }
        match self.pool_tab {
            PoolTab::Media => self.draw_media_pool_list(),
            PoolTab::Effects => self.draw_effects_panel(),
        }
        self.draw_panel_labels(w, media_w);
        self.draw_timeline_toolbar(w, top_h, canvas);
        self.draw_transport_bar(w, media_w, preview_w, preview_h, t);
        // Over every panel, since the drag crosses all of them on its way from
        // the library to a clip.
        self.draw_effect_drag();

        // --- Splitter handle: after the panels, so it sits over both it divides ---
        // A grabbed divider stays lit even once the cursor has run past a panel
        // minimum and left the line behind, which is the only feedback saying
        // the drag is still live and simply has nowhere further to go.
        let active = match self.drag {
            DragMode::Splitter(s) => Some(s),
            DragMode::None => self.splitter_at(self.cursor),
            _ => None,
        };
        match active {
            // Centered on the 1pt edge each panel already draws, so lighting up
            // moves nothing.
            Some(Splitter::TopBottom) => self.quads.push(Quad::colored(
                [0.0, top_h - (SPLITTER_ACTIVE_W - 1.0) * 0.5],
                [w, SPLITTER_ACTIVE_W],
                SPLITTER_ACTIVE_COLOR,
            )),
            Some(Splitter::PoolPreview) => self.quads.push(Quad::colored(
                [media_w - 1.0 - (SPLITTER_ACTIVE_W - 1.0) * 0.5, 0.0],
                [SPLITTER_ACTIVE_W, top_h],
                SPLITTER_ACTIVE_COLOR,
            )),
            None => {}
        }

        // --- Project settings popup: topmost, and first to be hit-tested ---
        if self.project_menu_open {
            self.draw_project_menu(self.timeline_project_btn, w, h);
        }
        self.draw_font_menu();
        // A picker whose clip is no longer the one shown closes, keeping
        // what it picked.
        if self.color_picker.is_some() && self.picker_layout().is_none() {
            self.close_color_picker();
        }
        self.draw_color_picker();

        let mut encoder = self.device.create_command_encoder(&Default::default());
        // Ahead of the main pass, which samples what these write.
        self.blur.encode(&self.device, &self.queue, &mut encoder);
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

            self.quads.draw(&self.device, &self.queue, &mut pass, [w, h]);
        }

        self.queue.submit([encoder.finish()]);
        if let Some(at) = self.eyedropper_pick.take() {
            if let Some(color) = self.read_pixel(&surface_texture.texture, at) {
                self.pick_color(color);
            }
        }
        self.window.pre_present_notify();
        surface_texture.present();
    }

    /// The colour of one pixel of the frame just drawn, for the eyedropper:
    /// what is on screen there, as the UI's own sRGB values. Waits for the
    /// GPU, which is fine once per click.
    fn read_pixel(&self, texture: &wgpu::Texture, [x, y]: [u32; 2]) -> Option<[f32; 4]> {
        if x >= texture.width() || y >= texture.height() {
            return None;
        }
        let bgra = match texture.format().remove_srgb_suffix() {
            wgpu::TextureFormat::Bgra8Unorm => true,
            wgpu::TextureFormat::Rgba8Unorm => false,
            other => {
                log::warn!("eyedropper: can't read a {other:?} surface");
                return None;
            }
        };
        let row = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("eyedropper"),
            size: row as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .ok()?;
        rx.recv().ok()?.ok()?;
        let px = buffer.slice(..4).get_mapped_range().to_vec();
        let [r, g, b] = if bgra { [px[2], px[1], px[0]] } else { [px[0], px[1], px[2]] };
        Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0])
    }

    /// The canvas, with every layer live at `t` composed onto it. Returns the
    /// resolved canvas, which the toolbar's gear tooltip also reports.
    ///
    /// The canvas is fitted into the panel first, then the layers are placed on
    /// the canvas — two stages, not one. Placing a clip straight into the panel
    /// is what used to hide format mismatches: a 4:3 clip filled a 16:9 preview
    /// edge to edge and then exported pillarboxed, with nothing on screen to
    /// warn you.
    ///
    /// Layers come from [`crate::compose::layers_at`], the same call the export
    /// makes, so what this panel shows is a scale model of the file rather than
    /// a second opinion about it.
    fn draw_preview(
        &mut self,
        media_w: f32,
        preview_w: f32,
        preview_h: f32,
        t: f64,
    ) -> Canvas {
        let canvas = self.canvas();
        let canvas_scale = (preview_w / canvas.width as f32)
            .min(preview_h / canvas.height as f32)
            .max(0.0);
        let canvas_w = (canvas.width as f32 * canvas_scale).round();
        let canvas_h = (canvas.height as f32 * canvas_scale).round();
        let canvas_x = (media_w + (preview_w - canvas_w) * 0.5).round();
        let canvas_y = ((preview_h - canvas_h) * 0.5).round();
        self.preview_canvas = Rect { x: canvas_x, y: canvas_y, w: canvas_w, h: canvas_h };
        self.preview_canvas_scale = canvas_scale;
        // True black, against a well light enough to read as a surround, so the
        // frame is visible as a frame even with nothing playing — that
        // rectangle is the only thing showing what shape the project is.
        self.quads.push(Quad::colored(
            [canvas_x, canvas_y],
            [canvas_w, canvas_h],
            CANVAS_COLOR,
        ));

        // Resolved before the loop below, which needs the pool mutably to
        // advance decoders — and cannot hold the immutable borrow this walk of
        // the timeline takes.
        let layers = self.frame_layers(t, canvas);
        // Titles are drawn rather than decoded, so their picture has to exist
        // before the pass that paints it. Baking is a no-op for anything whose
        // text and canvas have not moved since last frame.
        for layer in &layers {
            let Self { media, device, queue, quads, .. } = self;
            media.bake_title(
                layer.source,
                (canvas.width, canvas.height),
                device,
                queue,
                quads,
            );
        }
        // The pool row that lights up follows the top of the stack, which is
        // what "what you are looking at" means once there is more than one
        // picture on the canvas.
        self.last_playing_source = layers.last().map(|l| l.source);

        // Scoped disjoint borrows so the decoder advance + textured-quad push can
        // share this block without leaking borrows past it.
        {
            let Self { media, quads, queue, device, blur, .. } = self;
            for layer in &layers {
                let Some(quad) = canvas_quad(layer, canvas, canvas_x, canvas_y, canvas_scale)
                else {
                    continue;
                };
                // Which picture to draw. A title is baked into a picture with
                // its colours, outline and shadow already in it, so it draws
                // as an image does: its alpha rides through the blend as it
                // is, so a logo with a transparent background, or the space
                // around a title's letters, sits on whatever is below it.
                let texture = if let Some(texture) =
                    media.get(layer.source).and_then(|s| s.title_texture())
                {
                    Some(texture)
                } else if let Some(still) = media.get(layer.source).and_then(|s| s.still.as_ref()) {
                    Some(still.texture())
                } else if let Some(stream) =
                    media.get_mut(layer.source).and_then(|s| s.stream.as_mut())
                {
                    stream.goto(queue, layer.source_time);
                    Some(stream.texture())
                } else {
                    None
                };
                let Some(texture) = texture else {
                    continue;
                };
                // A blurred layer draws its blurred copy over the same rect:
                // the copy keeps the picture's shape, so the uv the placement
                // worked out holds for it unchanged.
                let texture = match layer.blur_plan(texture.height) {
                    Some(plan) => blur.prepare(device, quads, layer.clip, texture, plan),
                    None => texture,
                };
                quads.push_with(quad, Some(texture));
            }
        }

        self.draw_transform_handles();
        self.draw_title_edit_hint(canvas_y + canvas_h, media_w, preview_w);

        canvas
    }

    /// What the keyboard does while a title has the caret.
    ///
    /// Worth the line of chrome because typing takes the whole keyboard: every
    /// bare letter that normally edits the timeline is going into the text
    /// instead, and nothing else on screen would say why.
    fn draw_title_edit_hint(&mut self, below_y: f32, media_w: f32, preview_w: f32) {
        if !self.typing_title() {
            return;
        }
        // ASCII throughout, for the reason the media pool's format line is:
        // the UI font is a stock face rather than a subset we control.
        const HINT: &str = "Typing title - Enter for a new line, Esc or click elsewhere when done";
        let w = self.text.measure_width(HINT, STATUS_SIZE);
        let x = (media_w + (preview_w - w) * 0.5).round();
        let baseline = (below_y + LABEL_PAD + self.text.ascent(STATUS_SIZE)).round();
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [x, baseline],
            HINT,
            STATUS_SIZE,
            STATUS_INFO,
        );
    }

    /// The frame and corner boxes around the selected clip's picture.
    ///
    /// Drawn from the same rect the press is tested against, so what you can
    /// grab is what you can see — and only when there is something to grab: a
    /// clip that is selected but not live at the playhead has no picture on the
    /// canvas to put a frame around.
    fn draw_transform_handles(&mut self) {
        let Some((_, _, rect)) = self.preview_transform_target() else {
            return;
        };
        let b = TRANSFORM_OUTLINE_PX;
        for (pos, size) in [
            ([rect.x, rect.y], [rect.w, b]),
            ([rect.x, rect.y + rect.h - b], [rect.w, b]),
            ([rect.x, rect.y], [b, rect.h]),
            ([rect.x + rect.w - b, rect.y], [b, rect.h]),
        ] {
            self.quads
                .push(Quad::colored(pos, size, TRANSFORM_OUTLINE_COLOR));
        }
        for handle in transform_handle_rects(rect) {
            self.quads.push(Quad::colored(
                [handle.x, handle.y],
                [handle.w, handle.h],
                TRANSFORM_OUTLINE_COLOR,
            ));
        }
    }

    fn draw_media_pool_list(&mut self) {
        let grid = self.pool_view == PoolView::Grid;
        // Nothing scrubs while a drag is under way: a clip being carried to
        // the timeline is not being looked through.
        let idle = matches!(self.drag, DragMode::None);
        for (id, PoolCell { cell, thumb }) in self.pool_items() {
            let Some(src) = self.media.get(id) else {
                continue;
            };

            self.quads
                .push(Quad::colored([cell.x, cell.y], [cell.w, cell.h], POOL_ROW_COLOR));

            // Thumbnail slot (dark background so letterboxed thumbs look intentional).
            self.quads
                .push(Quad::colored([thumb.x, thumb.y], [thumb.w, thumb.h], POOL_THUMB_BG));

            // The poster frame, fitted into the slot preserving source aspect.
            // A hover swaps in the filmstrip sample for how far across the
            // slot the cursor is, so sweeping across it plays through the
            // clip — once the strip has been decoded; until then the poster
            // stays put.
            //
            // A source with no picture gets its waveform in the same slot
            // instead of an empty box or a stand-in icon: it is the one
            // thumbnail an audio file actually has, and it distinguishes a
            // music bed from a voiceover at a glance the way a frame does for
            // video.
            if let Some(video) = src.stream.as_ref() {
                let pic = fit_into(video.width() as f32, video.height() as f32, thumb);
                let scrub = src
                    .filmstrip
                    .as_ref()
                    .filter(|_| idle && thumb.contains(self.cursor))
                    .and_then(|strip| strip.frame_at((self.cursor[0] - pic.x) / pic.w));
                match scrub {
                    Some((atlas, uv)) => {
                        let quad = Quad { uv, ..Quad::textured([pic.x, pic.y], [pic.w, pic.h]) };
                        self.quads.push_with(quad, Some(atlas));
                        let line_x = self.cursor[0].clamp(pic.x, pic.x + pic.w - POOL_SCRUB_LINE_W);
                        self.quads.push(Quad::colored(
                            [line_x.round(), pic.y],
                            [POOL_SCRUB_LINE_W, pic.h],
                            POOL_SCRUB_LINE_COLOR,
                        ));
                    }
                    None => self.quads.push_with(
                        Quad::textured([pic.x, pic.y], [pic.w, pic.h]),
                        Some(video.thumbnail()),
                    ),
                }
            } else if let Some(still) = src.still.as_ref() {
                let pic = fit_into(still.width() as f32, still.height() as f32, thumb);
                self.quads.push_with(
                    Quad::textured([pic.x, pic.y], [pic.w, pic.h]),
                    Some(still.texture()),
                );
            } else if let Some(wf) = src.waveform.as_ref() {
                draw_waveform_summary(&mut self.quads, wf, thumb.x, thumb.y, thumb.w, thumb.h);
            } else if src.compound.is_some() {
                // The combine icon, standing for the clips inside: a
                // compound clip has no one picture of its own to show.
                let glyph = ICON_COMBINE.to_string();
                let size = POOL_COMPOUND_ICON_SIZE;
                let gw = self.text.measure_width(&glyph, size);
                let ascent = self.text.ascent(size);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [
                        (thumb.x + (thumb.w - gw) * 0.5).round(),
                        (thumb.y + (thumb.h + ascent) * 0.5).round(),
                    ],
                    &glyph,
                    size,
                    COMPOUND_LABEL_COLOR,
                );
            } else if let Some(title) = src.title.as_ref() {
                // A title's own words, in its own colour, standing in for the
                // frame a decoded source has — the same reasoning as the
                // waveform above: it is the one picture this source actually
                // has of itself.
                let [r, g, b, _] = title.color;
                let text = truncate_to_width(
                    &self.text,
                    title.pool_name(),
                    POOL_ITEM_META_SIZE,
                    thumb.w - 4.0,
                );
                let tw = self.text.measure_width(&text, POOL_ITEM_META_SIZE);
                let ascent = self.text.ascent(POOL_ITEM_META_SIZE);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [
                        (thumb.x + (thumb.w - tw) * 0.5).round(),
                        (thumb.y + (thumb.h + ascent) * 0.5).round(),
                    ],
                    &text,
                    POOL_ITEM_META_SIZE,
                    [r, g, b, 1.0],
                );
            }

            // Duration pill in the bottom-right of the thumb slot. A title or an
            // image has no length of its own to report, so it says what it is
            // instead.
            let dur_text = if src.title.is_some() {
                "TITLE".to_string()
            } else if src.still.is_some() {
                "IMAGE".to_string()
            } else {
                format_timecode(self.media.duration(id))
            };
            let dur_w = self.text.measure_width(&dur_text, POOL_ITEM_META_SIZE);
            let dur_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
            let pill_pad_x = 4.0;
            let pill_pad_y = 2.0;
            let pill_w = dur_w + pill_pad_x * 2.0;
            let pill_h = dur_ascent + pill_pad_y * 2.0;
            let pill_inset = 3.0;
            let pill_x = thumb.x + thumb.w - pill_inset - pill_w;
            let pill_y = thumb.y + thumb.h - pill_inset - pill_h;
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

            if grid {
                // Just the name, in the band under the picture: a tile is for
                // recognising footage, and the format line is a click away in
                // the list view.
                let name_ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
                let band_y = thumb.y + thumb.h;
                let baseline = (band_y + (POOL_TILE_NAME_H + name_ascent) * 0.5).round();
                let name = truncate_to_width(&self.text, &src.name, POOL_ITEM_NAME_SIZE, thumb.w);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [thumb.x, baseline],
                    &name,
                    POOL_ITEM_NAME_SIZE,
                    CLIP_LABEL_COLOR,
                );
            } else {
                self.draw_pool_row_text(id, cell, thumb);
            }

            if cell.contains(self.cursor) {
                let close = pool_row_close_rect(cell.x, cell.y, cell.w);
                let close_hover = close.contains(self.cursor);
                let bg = if close_hover {
                    POOL_CLOSE_BG_HOVER
                } else {
                    POOL_CLOSE_BG
                };
                self.quads
                    .push(Quad::colored([close.x, close.y], [close.w, close.h], bg));
                let glyph = ICON_CLOSE.to_string();
                let gw = self.text.measure_width(&glyph, POOL_CLOSE_LABEL_SIZE);
                let (gh, gymin) = self.text.glyph_visual_bounds(ICON_CLOSE, POOL_CLOSE_LABEL_SIZE);
                let gx = (close.x + (close.w - gw) * 0.5).round();
                let gy = (close.y + (close.h + gh) * 0.5 + gymin).round();
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [gx, gy],
                    &glyph,
                    POOL_CLOSE_LABEL_SIZE,
                    POOL_CLOSE_GLYPH_COLOR,
                );
            }
        }
    }

    /// A list row's name and format line, to the right of its thumbnail.
    fn draw_pool_row_text(&mut self, id: SourceId, cell: Rect, thumb: Rect) {
        let Some(src) = self.media.get(id) else {
            return;
        };
        // The pair is centred as a block. Both clamp with an ellipsis rather
        // than bleeding into the preview — a narrow pool loses the tail of the
        // filename, which is the half worth losing.
        //
        // The format is worth the second line because it's what decides
        // whether a clip matches the canvas, and `Setting::Auto` means the
        // canvas is inherited from one of these rows: without it, the only
        // way to see what you're inheriting is to look at the export.
        let name_x = thumb.x + thumb.w + POOL_ROW_PAD + 4.0;
        let name_max_w = (cell.x + cell.w - POOL_ROW_PAD - name_x).max(0.0);
        let name_ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
        let meta_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
        let block_h = name_ascent + POOL_ITEM_META_GAP + meta_ascent;
        let name_baseline = (cell.y + (cell.h - block_h) * 0.5 + name_ascent).round();
        let meta_baseline = name_baseline + POOL_ITEM_META_GAP + meta_ascent;
        let name = truncate_to_width(&self.text, &src.name, POOL_ITEM_NAME_SIZE, name_max_w);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [name_x, name_baseline],
            &name,
            POOL_ITEM_NAME_SIZE,
            CLIP_LABEL_COLOR,
        );

        // ASCII throughout: the UI font is a stock TTF rather than a subset
        // we control, so a '×' or '·' that turned out to be missing would
        // fail as a blank rather than as a build error.
        let meta = match (src.still.as_ref(), src.stream.as_ref(), src.audio.as_ref()) {
            // An image sets the canvas no more than a title does, but whether
            // it matches the canvas is the same question footage answers here.
            (Some(s), _, _) => format!("{}x{} image", s.width(), s.height()),
            (None, Some(v), _) => format!(
                "{}x{} @ {} fps",
                v.width(),
                v.height(),
                fmt_fps(v.frame_rate()),
            ),
            // The same question one line down for a source with no
            // picture: not what canvas it would set, but what it is.
            (None, None, Some(a)) => format!(
                "{} Hz {}",
                a.src_rate(),
                match a.src_channels() {
                    1 => "mono".to_string(),
                    2 => "stereo".to_string(),
                    n => format!("{n} ch"),
                }
            ),
            (None, None, None) => match src.title.as_ref() {
                // What a title will be drawn at, which is the same question
                // the format line answers for footage: whether it matches
                // the canvas. A title always does — it is drawn at whatever
                // the canvas turns out to be — so it says its size instead.
                Some(t) => format!("title, {}% of frame", (t.size * 100.0).round()),
                // A compound clip says what it holds.
                None => match src.compound.as_ref() {
                    Some(tracks) => {
                        let n: usize = tracks.iter().map(|(_, c)| c.len()).sum();
                        format!("compound clip, {n} {}", if n == 1 { "clip" } else { "clips" })
                    }
                    None => String::new(),
                },
            },
        };
        let meta = truncate_to_width(&self.text, &meta, POOL_ITEM_META_SIZE, name_max_w);
        self.text.draw(
            &self.queue,
            &mut self.quads,
            [name_x, meta_baseline],
            &meta,
            POOL_ITEM_META_SIZE,
            POOL_ITEM_META_COLOR,
        );
    }
}

/// A `sw x sh` picture fitted into `slot` preserving its aspect, centred and
/// snapped to whole points.
fn fit_into(sw: f32, sh: f32, slot: Rect) -> Rect {
    let scale = (slot.w / sw.max(1.0)).min(slot.h / sh.max(1.0));
    let (w, h) = ((sw * scale).round(), (sh * scale).round());
    Rect {
        x: (slot.x + (slot.w - w) * 0.5).round(),
        y: (slot.y + (slot.h - h) * 0.5).round(),
        w,
        h,
    }
}

