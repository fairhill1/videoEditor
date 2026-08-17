//! Drawing the timeline panel: the ruler, the track lanes, the clips on them,
//! the playhead and the drag ghost.
//!
//! `timeline.rs` is the model; this is the picture of it. Positions here are
//! recomputed rather than taken from [`crate::layout::TimelineLayout`] on
//! purpose — the drawn width of a lane folds in the duration of a clip being
//! dragged in from the pool, which the hit-testing layout must not, or a clip
//! would land somewhere other than where its ghost showed.

use crate::fmt::{fmt_db, format_tick_label, nice_tick_interval, truncate_to_width};
use crate::input::DragMode;
use crate::layout::{
    compute_lane_height, fade_handle_rect, gain_to_y, lane_y, topmost_lane_top,
};
use crate::quad::{Quad, QuadRenderer};
use crate::state::State;
use crate::theme::*;
use crate::timeline::{gain_to_db, FadeSide, TrackKind};

/// Shade away the part of a clip its fade attenuates.
///
/// One 1pt column per pixel of the fade's width, each reaching down from the
/// top of the lane by however much level the ramp has taken out there. The same
/// staircase the waveform is drawn with, and for the same reason: the renderer
/// draws rectangles, and a fade is a diagonal. The bright edge along the bottom
/// of the shading is what reads as the fade line itself.
fn draw_fade_wedge(
    quads: &mut QuadRenderer,
    x: f32,
    w: f32,
    lane_y: f32,
    lane_h: f32,
    side: FadeSide,
) {
    if w <= 0.0 {
        return;
    }
    for col in 0..w.ceil() as i32 {
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

impl State {
    /// Everything below the timeline splitter except its toolbar: background,
    /// ruler, lanes, playhead and ghost, in painter's order.
    pub(crate) fn draw_timeline_panel(&mut self, w: f32, h: f32, top_h: f32, t: f64) {
        let bottom_h = h - top_h;
        self.quads
            .push(Quad::colored([0.0, top_h], [w, bottom_h], TIMELINE_COLOR));
        self.quads
            .push(Quad::colored([0.0, top_h], [w, 1.0], PANEL_BORDER_COLOR));

        let tracks_top = top_h + TIMELINE_TOP_PAD;
        let tracks_bottom = h;
        let tracks_area_h = (tracks_bottom - tracks_top).max(0.0);
        // Snap center to a whole pixel so derived lane_y values don't land on
        // half-pixels (which renders as a blurry edge under bilinear sampling).
        let center_y = (tracks_top + tracks_area_h * 0.5).round();
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
        self.quads
            .push(Quad::colored([0.0, center_y - 0.5], [w, 1.0], DIVIDER_COLOR));

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
        let pps = clips_w / timeline_duration_display as f32;
        let interval = nice_tick_interval(pps);
        let label_size = TIMELINE_RULER_LABEL_SIZE;
        let label_ascent = self.text.ascent(label_size);
        let label_baseline = (ruler_top + label_ascent + 3.0).round();
        let mut i: usize = 0;
        loop {
            let tick_t = i as f64 * interval;
            if tick_t > timeline_duration_display {
                break;
            }
            let x = (clips_x + (tick_t / timeline_duration_display) as f32 * clips_w).round();
            self.quads.push(Quad::colored(
                [x, ruler_bottom - TIMELINE_RULER_TICK_H],
                [1.0, TIMELINE_RULER_TICK_H],
                TIMELINE_RULER_TICK_COLOR,
            ));
            let label = format_tick_label(tick_t, interval);
            let lw = self.text.measure_width(&label, label_size);
            let lx = (x + 3.0).round();
            if lx + lw <= clips_x + clips_w {
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [lx, label_baseline],
                    &label,
                    label_size,
                    TIMELINE_RULER_LABEL_COLOR,
                );
            }
            i += 1;
            if i > 10_000 {
                break;
            }
        }

        // V1 sits just above the divider (leaving half_gap between its bottom
        // and center_y), V2 stacks above V1 with a full TRACK_LANE_GAP between.
        for (visual_i, &track_idx) in video_tracks.iter().enumerate() {
            let lane_y = lane_y(center_y, lane_h, visual_i, TrackKind::Video);
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
            let lane_y = lane_y(center_y, lane_h, visual_i, TrackKind::Audio);
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
        // Starts at the ruler rather than the top of the panel: it marks a
        // position on the time scale, and the toolbar above has no time axis
        // for it to point at.
        if self.timeline.duration() > 0.0 {
            let ratio = (t / self.timeline.duration()).clamp(0.0, 1.0) as f32;
            let px = (clips_x + ratio * clips_w - PLAYHEAD_WIDTH * 0.5).round();
            self.quads.push(Quad::colored(
                [px, ruler_top],
                [PLAYHEAD_WIDTH, h - ruler_top],
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
            self.quads
                .push(Quad::colored([gx, gy], [ghost_w, ghost_h], ghost_color));
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
        let unselected_border = darken(clip_color, CLIP_BORDER_DARKEN);

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
                            let n_cols = cw.ceil() as i32;
                            let n_peaks = wf.peaks.len();
                            for col in 0..n_cols {
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
                let px_per_sec = clips_w / timeline_duration as f32;
                if clip.fade_in > 0.0 {
                    let fw = clip.fade_in as f32 * px_per_sec;
                    draw_fade_wedge(&mut self.quads, x, fw, lane_y, lane_h, FadeSide::In);
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

                // Drawn through the same rect the hit test uses, so the box
                // you can see is the box you can grab.
                let px_per_sec = clips_w / timeline_duration as f32;
                for at in [
                    clip.fade_in as f32 * px_per_sec,
                    cw - clip.fade_out as f32 * px_per_sec,
                ] {
                    let r = fade_handle_rect(x + at, x, x + cw, lane_y);
                    self.quads
                        .push(Quad::colored([r.x, r.y], [r.w, r.h], CLIP_FADE_HANDLE_COLOR));
                }

                // The number only earns its space once the level has been
                // moved, or while the clip is selected and you are moving it.
                let db = gain_to_db(clip.gain);
                if selected || db.abs() > 0.05 {
                    let text = fmt_db(db);
                    let tw = self.text.measure_width(&text, CLIP_LEVEL_LABEL_SIZE);
                    if tw + CLIP_LEVEL_LABEL_PAD * 2.0 <= cw {
                        // Below the line, and pulled back inside the lane when
                        // the line has been dragged near the bottom of it.
                        let baseline = (level_y + self.text.ascent(CLIP_LEVEL_LABEL_SIZE) + 2.0)
                            .min(lane_y + lane_h - 2.0);
                        self.text.draw(
                            &self.queue,
                            &mut self.quads,
                            [(x + cw - CLIP_LEVEL_LABEL_PAD - tw).round(), baseline.round()],
                            &text,
                            CLIP_LEVEL_LABEL_SIZE,
                            CLIP_LEVEL_LABEL_COLOR,
                        );
                    }
                }
            }

            if let Some(src) = self.media.get(clip.source) {
                let label_pad = 6.0;
                let label_max_w = (cw - label_pad * 2.0).max(0.0);
                let label_baseline = lane_y + self.text.ascent(CLIP_LABEL_SIZE) + 4.0;
                let name = truncate_to_width(&self.text, &src.name, CLIP_LABEL_SIZE, label_max_w);
                self.text.draw(
                    &self.queue,
                    &mut self.quads,
                    [x + label_pad, label_baseline],
                    &name,
                    CLIP_LABEL_SIZE,
                    CLIP_LABEL_COLOR,
                );
            }
        }
    }
}
