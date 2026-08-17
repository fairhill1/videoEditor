//! One frame, start to finish.
//!
//! [`State::render`] is the only place the drawing order is decided. Quads are
//! painted back to front with no depth buffer, so the sequence of calls here
//! *is* the layering: panels, then their contents, then the chrome on top, then
//! the splitter handle and the popup over everything.

use crate::canvas::Canvas;
use crate::fmt::{fmt_fps, format_timecode, truncate_to_width};
use crate::input::DragMode;
use crate::layout::{pool_row_close_rect, Splitter};
use crate::audio::Waveform;
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::theme::*;

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
            let Self {
                audio,
                timeline,
                media,
                ..
            } = self;
            audio.tick(timeline, media);
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
        self.draw_timeline_panel(w, h, top_h, t);
        self.draw_media_pool_list(media_w, top_h);
        self.draw_panel_labels(w, media_w);
        self.draw_timeline_toolbar(w, top_h, canvas);
        self.draw_transport_bar(w, media_w, preview_w, preview_h, t);

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

            self.quads.draw(&self.device, &self.queue, &mut pass, [w, h]);
        }

        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        surface_texture.present();
    }

    /// The canvas, and the topmost active clip composed onto it. Returns the
    /// resolved canvas, which the toolbar's gear tooltip also reports.
    ///
    /// The canvas is fitted into the panel first, then the clip is fitted into
    /// the canvas — two stages, not one. Fitting the clip straight to the panel
    /// is what used to hide format mismatches: a 4:3 clip filled a 16:9 preview
    /// edge to edge and then exported pillarboxed, with nothing on screen to
    /// warn you.
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
        // Black, unlike the near-black panel around it, so the frame is visible
        // as a frame even with nothing playing — that outline is the only thing
        // showing what shape the project is.
        self.quads.push(Quad::colored(
            [canvas_x, canvas_y],
            [canvas_w, canvas_h],
            CANVAS_COLOR,
        ));

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
                if let Some(stream) = media.get_mut(source_id).and_then(|s| s.stream.as_mut()) {
                    stream.goto(queue, source_t);

                    // Placement is computed in canvas pixels and then scaled to
                    // the panel, rather than fitted to the panel directly, so
                    // the preview is a faithful scale model of the export.
                    let (cx, cy, cw, ch) =
                        canvas.fit(stream.width() as f32, stream.height() as f32);
                    quads.push_with(
                        Quad::textured(
                            [canvas_x + cx * canvas_scale, canvas_y + cy * canvas_scale],
                            [cw * canvas_scale, ch * canvas_scale],
                        ),
                        Some(stream.texture()),
                    );
                }
            } else {
                *last_playing_source = None;
            }
        }

        canvas
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
            // A source with no picture gets its waveform in the same slot
            // instead of an empty box or a stand-in icon: it is the one
            // thumbnail an audio file actually has, and it distinguishes a
            // music bed from a voiceover at a glance the way a frame does for
            // video.
            if let Some(video) = src.stream.as_ref() {
                let thumb = video.thumbnail();
                let tw = thumb.width as f32;
                let th = thumb.height as f32;
                let scale = (POOL_THUMB_W / tw).min(POOL_THUMB_H / th);
                let dw = (tw * scale).round();
                let dh = (th * scale).round();
                let dx = (slot_x + (POOL_THUMB_W - dw) * 0.5).round();
                let dy = (slot_y + (POOL_THUMB_H - dh) * 0.5).round();
                self.quads
                    .push_with(Quad::textured([dx, dy], [dw, dh]), Some(thumb));
            } else if let Some(wf) = src.waveform.as_ref() {
                draw_waveform_summary(
                    &mut self.quads,
                    wf,
                    slot_x,
                    slot_y,
                    POOL_THUMB_W,
                    POOL_THUMB_H,
                );
            }

            // Duration pill in the bottom-right of the thumb slot.
            let dur_text = format_timecode(self.media.duration(id));
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

            // Name and format line to the right of the thumb, the pair centered
            // as a block. Both clamp with an ellipsis rather than bleeding into
            // the preview — a narrow pool loses the tail of the filename, which
            // is the half worth losing.
            //
            // The format is worth the second line because it's what decides
            // whether a clip matches the canvas, and `Setting::Auto` means the
            // canvas is inherited from one of these rows: without it, the only
            // way to see what you're inheriting is to look at the export.
            let name_x = slot_x + POOL_THUMB_W + POOL_ROW_PAD + 4.0;
            let name_max_w = (row_x + row_w - POOL_ROW_PAD - name_x).max(0.0);
            let name_ascent = self.text.ascent(POOL_ITEM_NAME_SIZE);
            let meta_ascent = self.text.ascent(POOL_ITEM_META_SIZE);
            let block_h = name_ascent + POOL_ITEM_META_GAP + meta_ascent;
            let name_baseline = (row_y + (POOL_ROW_HEIGHT - block_h) * 0.5 + name_ascent).round();
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
            let meta = match (src.stream.as_ref(), src.audio.as_ref()) {
                (Some(v), _) => format!(
                    "{}x{} @ {} fps",
                    v.width(),
                    v.height(),
                    fmt_fps(v.frame_rate()),
                ),
                // The same question one line down for a source with no
                // picture: not what canvas it would set, but what it is.
                (None, Some(a)) => format!(
                    "{} Hz {}",
                    a.src_rate(),
                    match a.src_channels() {
                        1 => "mono".to_string(),
                        2 => "stereo".to_string(),
                        n => format!("{n} ch"),
                    }
                ),
                (None, None) => String::new(),
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

            let row_hovered = self.cursor[0] >= row_x
                && self.cursor[0] <= row_x + row_w
                && self.cursor[1] >= row_y
                && self.cursor[1] <= row_y + POOL_ROW_HEIGHT;
            if row_hovered {
                let close = pool_row_close_rect(row_x, row_y, row_w);
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
}
