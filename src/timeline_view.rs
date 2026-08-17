//! Drawing the timeline panel: the ruler, the track lanes, the clips on them,
//! the playhead and the drag ghost.
//!
//! `timeline.rs` is the model; this is the picture of it. Every position comes
//! from [`crate::layout::TimelineLayout`] rather than being worked out again
//! here, so the rect you can see is the rect the hit test hands back — and so
//! zooming is a change to the view window alone rather than to every site that
//! draws.

use crate::fmt::{fmt_db, format_tick_label, nice_tick_interval, truncate_to_width};
use crate::input::DragMode;
use crate::layout::{fade_handle_rect, gain_to_y, lane_y, topmost_lane_top, TimelineLayout};
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::text::TextRenderer;
use crate::theme::*;
use crate::timeline::{gain_to_db, FadeSide, TrackKind};

/// Shade away the part of a clip its fade attenuates.
///
/// One 1pt column per pixel of the fade's width, each reaching down from the
/// top of the lane by however much level the ramp has taken out there. The same
/// staircase the waveform is drawn with, and for the same reason: the renderer
/// draws rectangles, and a fade is a diagonal. The bright edge along the bottom
/// of the shading is what reads as the fade line itself.
///
/// `band` is the visible stretch of the lane. The renderer would drop the
/// columns outside it anyway, but a fade is thousands of columns wide once the
/// timeline is zoomed in far enough, and it is the loop that costs.
fn draw_fade_wedge(
    quads: &mut QuadRenderer,
    x: f32,
    w: f32,
    lane_y: f32,
    lane_h: f32,
    side: FadeSide,
    band: [f32; 2],
) {
    if w <= 0.0 {
        return;
    }
    let first = (band[0] - x).max(0.0).floor() as i32;
    let last = (band[1] - x).min(w).ceil() as i32;
    for col in first..last {
        let along = (col as f32 + 0.5) / w;
        // How much of the level is still passing at this column.
        let level = match side {
            FadeSide::In => along,
            FadeSide::Out => 1.0 - along,
        }
        .clamp(0.0, 1.0);
        let cut_h = lane_h * (1.0 - level);
        let px = x + col as f32;
        quads.push(Quad::colored([px, lane_y], [1.0, cut_h], CLIP_FADE_SHADE));
        quads.push(Quad::colored(
            [px, lane_y + cut_h],
            [1.0, CLIP_FADE_EDGE_H],
            CLIP_FADE_EDGE_COLOR,
        ));
    }
}

/// Draw a label on a plate, for text that has to stay readable over whatever
/// the clip beneath it happens to be showing.
///
/// `pos` is the baseline of the first glyph, as [`TextRenderer::draw`] takes
/// it. The plate is measured from the same string, at the same size, that the
/// text is then drawn with, so the two cannot disagree about how much of the
/// clip to cover.
///
/// A free function rather than a method on `State`: its callers are part-way
/// through a borrow of the timeline they are drawing, and naming the three
/// fields it needs is what lets it be called from there at all.
fn draw_plated_label(
    queue: &wgpu::Queue,
    text: &mut TextRenderer,
    quads: &mut QuadRenderer,
    pos: [f32; 2],
    label: &str,
    size: f32,
    color: [f32; 4],
) {
    if label.is_empty() {
        return;
    }
    let w = text.measure_width(label, size);
    let (ascent, descent) = (text.ascent(size), text.descent(size));
    quads.push(Quad::colored(
        [
            (pos[0] - CLIP_LABEL_PLATE_PAD_X).round(),
            (pos[1] - ascent - CLIP_LABEL_PLATE_PAD_Y).round(),
        ],
        [
            (w + CLIP_LABEL_PLATE_PAD_X * 2.0).round(),
            (ascent + descent + CLIP_LABEL_PLATE_PAD_Y * 2.0).round(),
        ],
        CLIP_LABEL_PLATE_COLOR,
    ));
    text.draw(queue, quads, pos, label, size, color);
}

impl State {
    /// Everything below the timeline splitter except its toolbar: background,
    /// ruler, lanes, playhead and ghost, in painter's order.
    pub(crate) fn draw_timeline_panel(&mut self, w: f32, h: f32, top_h: f32, t: f64) {
        let bottom_h = h - top_h;
        self.quads
            .push(Quad::colored([0.0, top_h], [w, bottom_h], TIMELINE_COLOR));
        self.quads
            .push(Quad::colored([0.0, top_h], [w, 1.0], PANEL_BORDER_COLOR));

        let layout = self.timeline_layout();
        let (center_y, lane_h) = (layout.center_y, layout.lane_h);
        let band = [layout.clips_x, layout.clips_right()];

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
        self.quads
            .push(Quad::colored([0.0, center_y - 0.5], [w, 1.0], DIVIDER_COLOR));

        // --- Timeline ruler: flush above the topmost video lane, with tick
        //     marks + timecode labels along the bottom edge so the scale is
        //     legible at a glance.
        let topmost = topmost_lane_top(center_y, lane_h, video_tracks.len());
        let ruler_bottom = topmost;
        let ruler_top = ruler_bottom - TIMELINE_RULER_H;
        self.quads.push(Quad::colored(
            [0.0, ruler_top],
            [w, TIMELINE_RULER_H],
            TIMELINE_RULER_COLOR,
        ));
        // Panel-weight, not divider-weight: this is where the ruler's chrome
        // ends and the clip wells begin.
        self.quads.push(Quad::colored(
            [0.0, ruler_bottom - 1.0],
            [w, 1.0],
            PANEL_BORDER_COLOR,
        ));
        let interval = nice_tick_interval(layout.px_per_sec() as f32);
        let label_size = TIMELINE_RULER_LABEL_SIZE;
        let label_ascent = self.text.ascent(label_size);
        let label_baseline = (ruler_top + label_ascent + 3.0).round();
        // Only the ticks the view can show, so the loop is bounded by the
        // panel's width rather than by how long the project is: the interval is
        // chosen to put one every hundred points or so, whatever the zoom.
        let first_tick = (layout.view_start / interval).floor().max(0.0) as i64;
        let last_tick = ((layout.view_start + layout.view_dur) / interval).floor() as i64;
        self.quads.set_clip_x(band[0], band[1]);
        for i in first_tick..=last_tick {
            let tick_t = i as f64 * interval;
            let x = layout.t_to_x(tick_t).round();
            self.quads.push(Quad::colored(
                [x, ruler_bottom - TIMELINE_RULER_TICK_H],
                [1.0, TIMELINE_RULER_TICK_H],
                TIMELINE_RULER_TICK_COLOR,
            ));
            let label = format_tick_label(tick_t, interval);
            let lw = self.text.measure_width(&label, label_size);
            let lx = (x + 3.0).round();
            // Whole or not at all: a label the band would cut through reads as
            // a different time than the one it belongs to.
            if lx >= band[0] && lx + lw <= band[1] {
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [lx, label_baseline],
                    &label,
                    label_size,
                    TIMELINE_RULER_LABEL_COLOR,
                );
            }
        }
        self.quads.clear_clip_x();

        // V1 sits just above the divider (leaving half_gap between its bottom
        // and center_y), V2 stacks above V1 with a full TRACK_LANE_GAP between.
        for (visual_i, &track_idx) in video_tracks.iter().enumerate() {
            let lane_y = lane_y(center_y, lane_h, visual_i, TrackKind::Video);
            self.draw_track_lane(layout, lane_y, track_idx, visual_i);
        }
        // A1 sits just below the divider, A2 below A1, etc.
        for (visual_i, &track_idx) in audio_tracks.iter().enumerate() {
            let lane_y = lane_y(center_y, lane_h, visual_i, TrackKind::Audio);
            self.draw_track_lane(layout, lane_y, track_idx, visual_i);
        }

        // --- Playhead: drawn last so it's on top of clips ---
        // Starts at the ruler rather than the top of the panel: it marks a
        // position on the time scale, and the toolbar above has no time axis
        // for it to point at. Zoomed in it can be off the edge entirely, which
        // the band takes care of.
        let scroll = layout.scrollbar_rect();
        if self.timeline.duration() > 0.0 {
            let px = (layout.t_to_x(t) - PLAYHEAD_WIDTH * 0.5).round();
            self.quads.set_clip_x(band[0], band[1]);
            self.quads.push(Quad::colored(
                [px, ruler_top],
                // Stops at the scroll strip: the playhead marks a position on
                // the time axis, and the strip below measures the view rather
                // than being part of it.
                [PLAYHEAD_WIDTH, scroll.y - ruler_top],
                PLAYHEAD_COLOR,
            ));
            self.quads.clear_clip_x();
        }

        // --- Pool-drag ghost: previews where the clip will land ---
        // Start-aligned to the cursor (matches `end_drag`'s drop_t semantics)
        // and snapped to the hovered lane's y when over one, so the preview
        // rect is exactly the rect that'll be created on mouse-up. Its duration
        // is already folded into the view (see `State::content_duration`), so
        // the width here is the width it will have once dropped.
        if let DragMode::PoolDrag { source } = self.drag {
            let dur = self.media.duration(source);
            let ghost_w = ((dur * layout.px_per_sec()) as f32).max(GHOST_MIN_W);
            let ghost_h = lane_h;
            let over_lane = self.track_at_y(self.cursor[1], &layout);
            let gx = self.cursor[0].max(layout.clips_x);
            let (gy, ghost_color) = match over_lane {
                Some(track_idx) => match self.timeline.tracks[track_idx].kind {
                    TrackKind::Video => {
                        let visual_i =
                            video_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        (
                            lane_y(center_y, lane_h, visual_i, TrackKind::Video),
                            DRAG_GHOST_VIDEO_COLOR,
                        )
                    }
                    TrackKind::Audio => {
                        let visual_i =
                            audio_tracks.iter().position(|&i| i == track_idx).unwrap_or(0);
                        (
                            lane_y(center_y, lane_h, visual_i, TrackKind::Audio),
                            DRAG_GHOST_AUDIO_COLOR,
                        )
                    }
                },
                None => (self.cursor[1] - ghost_h * 0.5, DRAG_GHOST_VIDEO_COLOR),
            };
            self.quads.set_clip_x(band[0], band[1]);
            self.quads
                .push(Quad::colored([gx, gy], [ghost_w, ghost_h], ghost_color));
            self.quads.clear_clip_x();
        }

        // --- Scrollbar: last, so a ghost dragged over it passes behind ---
        // The well is always there, which is what says the timeline scrolls at
        // all; the thumb appears once there is more timeline than view.
        self.quads.push(Quad::colored(
            [scroll.x, scroll.y],
            [scroll.w, scroll.h],
            SCROLLBAR_TRACK_COLOR,
        ));
        if let Some(thumb) = layout.scroll_thumb() {
            // Hover only counts when nothing else is being dragged: passing the
            // cursor over the thumb while trimming a clip is not an offer to
            // grab it, and lighting up would say it was.
            let color = match self.drag {
                DragMode::Scrollbar { .. } => SCROLLBAR_THUMB_ACTIVE_COLOR,
                DragMode::None if thumb.contains(self.cursor) => SCROLLBAR_THUMB_HOVER_COLOR,
                _ => SCROLLBAR_THUMB_COLOR,
            };
            self.quads.push(Quad::colored(
                [thumb.x.round(), thumb.y],
                [thumb.w.round(), thumb.h],
                color,
            ));
        }
    }

    fn draw_track_lane(
        &mut self,
        layout: TimelineLayout,
        lane_y: f32,
        track_idx: usize,
        visual_i: usize,
    ) {
        let lane_h = layout.lane_h;
        let band = [layout.clips_x, layout.clips_right()];
        let track = &self.timeline.tracks[track_idx];
        let (clip_color, label_prefix) = match track.kind {
            TrackKind::Video => (VIDEO_CLIP_COLOR, "V"),
            TrackKind::Audio => (AUDIO_CLIP_COLOR, "A"),
        };
        let unselected_border = darken(clip_color, CLIP_BORDER_DARKEN);

        // Lane background and header, outside the clip band: the header is the
        // one part of a lane that doesn't move when the timeline scrolls.
        self.quads.push(Quad::colored(
            [0.0, lane_y],
            [layout.clips_right(), lane_h],
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
        self.quads.set_clip_x(band[0], band[1]);
        for clip in &track.clips {
            let x = layout.t_to_x(clip.timeline_start);
            let x_end = layout.t_to_x(clip.timeline_end());
            // Skipped here rather than left to the renderer's own clipping: the
            // waveform and fade loops below run once per pixel of clip width,
            // and zoomed in far enough that is a great many pixels of nothing.
            if x_end < band[0] || x > band[1] {
                continue;
            }
            let cw = (x_end - x).max(1.0);
            // The part of the clip actually on screen. Labels are placed
            // against this rather than against the clip, so a clip running off
            // an edge still says what it is.
            let vis_x0 = x.max(band[0]);
            let vis_x1 = x_end.min(band[1]);
            let vis_w = (vis_x1 - vis_x0).max(0.0);
            // Selection reads as a brighter version of the clip plus an accent
            // outline, rather than a colour of its own: the blue/green split
            // between video and audio is load-bearing, so recolouring the fill
            // outright would cost more information than it gives.
            let selected = self.selected == Some(clip.id);
            let (fill, border_color, b) = if selected {
                (
                    lighten(clip_color, CLIP_SELECTED_LIFT),
                    CLIP_SELECTED_BORDER,
                    CLIP_SELECTED_BORDER_PX,
                )
            } else {
                (clip_color, unselected_border, CLIP_BORDER_PX)
            };
            self.quads
                .push(Quad::colored([x, lane_y], [cw, lane_h], fill));

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
                            let first_col = (vis_x0 - x).max(0.0).floor() as i32;
                            let last_col = (vis_x1 - x).min(cw).ceil() as i32;
                            let n_peaks = wf.peaks.len();
                            for col in first_col..last_col {
                                let src_t_start = clip.source_in + col as f64 * seconds_per_px;
                                let src_t_end = src_t_start + seconds_per_px;
                                let idx_start = (src_t_start / wf.bucket_seconds) as usize;
                                let mut idx_end = ((src_t_end / wf.bucket_seconds).ceil() as usize)
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

            // Fades, between the waveform and the outline: the wedge darkens
            // the peaks it is quietening, and the outline still closes over the
            // top of it.
            if track.kind == TrackKind::Audio && cw > 1.0 {
                let px_per_sec = layout.px_per_sec() as f32;
                if clip.fade_in > 0.0 {
                    let fw = clip.fade_in as f32 * px_per_sec;
                    draw_fade_wedge(&mut self.quads, x, fw, lane_y, lane_h, FadeSide::In, band);
                }
                if clip.fade_out > 0.0 {
                    let fw = clip.fade_out as f32 * px_per_sec;
                    draw_fade_wedge(
                        &mut self.quads,
                        x + cw - fw,
                        fw,
                        lane_y,
                        lane_h,
                        FadeSide::Out,
                        band,
                    );
                }
            }

            // Outline, drawn after the waveform so peaks can't paint over it.
            // Every clip gets one, so two butt-joined clips — the halves of a
            // split — meet in a 2px seam. Deliberately not a gap: a split
            // leaves no gap, and faking one would look identical to genuine
            // empty space between clips. For the same reason the outline is a
            // darkened tint of the clip rather than the lane color, so a seam
            // stays distinguishable from a real gap.
            self.quads
                .push(Quad::colored([x, lane_y], [cw, b], border_color));
            self.quads.push(Quad::colored(
                [x, lane_y + lane_h - b],
                [cw, b],
                border_color,
            ));
            // A sliver narrower than its own two edges would render as solid
            // border; leave it as plain clip color instead.
            if cw >= b * 3.0 {
                self.quads
                    .push(Quad::colored([x, lane_y], [b, lane_h], border_color));
                self.quads.push(Quad::colored(
                    [x + cw - b, lane_y],
                    [b, lane_h],
                    border_color,
                ));
            }

            // Level line and fade handles, over the outline: they are controls
            // rather than part of the clip's body, and one hidden under an edge
            // is one you cannot find.
            if track.kind == TrackKind::Audio && cw > 1.0 {
                let level_y = gain_to_y(lane_y, lane_h, clip.gain);
                let (line_color, line_h) = if selected {
                    (CLIP_LEVEL_ACTIVE_COLOR, CLIP_LEVEL_ACTIVE_H)
                } else {
                    (CLIP_LEVEL_COLOR, CLIP_LEVEL_LINE_H)
                };
                self.quads.push(Quad::colored(
                    [x, (level_y - line_h * 0.5).round()],
                    [cw, line_h],
                    line_color,
                ));

                // The number only earns its space once the level has been
                // moved, or while the clip is selected and you are moving it.
                let db = gain_to_db(clip.gain);
                if selected || db.abs() > 0.05 {
                    let text = fmt_db(db);
                    let tw = self.text.measure_width(&text, CLIP_LEVEL_LABEL_SIZE);
                    if tw + CLIP_LEVEL_LABEL_PAD * 2.0 <= vis_w {
                        // Below the line, and pulled back inside the lane when
                        // the line has been dragged near the bottom of it.
                        let baseline = (level_y + self.text.ascent(CLIP_LEVEL_LABEL_SIZE) + 2.0)
                            .min(lane_y + lane_h - 2.0);
                        draw_plated_label(
                            &self.queue,
                            &mut self.text,
                            &mut self.quads,
                            [(vis_x1 - CLIP_LEVEL_LABEL_PAD - tw).round(), baseline.round()],
                            &text,
                            CLIP_LEVEL_LABEL_SIZE,
                            CLIP_LEVEL_LABEL_COLOR,
                        );
                    }
                }
            }

            if let Some(src) = self.media.get(clip.source) {
                let label_pad = 6.0;
                let label_max_w = (vis_w - label_pad * 2.0).max(0.0);
                let label_baseline = lane_y + self.text.ascent(CLIP_LABEL_SIZE) + 4.0;
                let name = truncate_to_width(&self.text, &src.name, CLIP_LABEL_SIZE, label_max_w);
                draw_plated_label(
                    &self.queue,
                    &mut self.text,
                    &mut self.quads,
                    [vis_x0 + label_pad, label_baseline],
                    &name,
                    CLIP_LABEL_SIZE,
                    CLIP_LABEL_COLOR,
                );
            }

            // Fade handles last of all, over the name and its plate: the head
            // handle sits in the same corner the name starts in, and between a
            // control you drag and a caption that names the clip, the control
            // is the one that has to stay findable.
            //
            // Drawn through the same rect the hit test uses, so the box you can
            // see is the box you can grab.
            if track.kind == TrackKind::Audio && cw > 1.0 {
                let px_per_sec = layout.px_per_sec() as f32;
                for at in [
                    clip.fade_in as f32 * px_per_sec,
                    cw - clip.fade_out as f32 * px_per_sec,
                ] {
                    let r = fade_handle_rect(x + at, x, x + cw, lane_y);
                    self.quads
                        .push(Quad::colored([r.x, r.y], [r.w, r.h], CLIP_FADE_HANDLE_COLOR));
                }
            }
        }
        self.quads.clear_clip_x();
    }
}
